use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, DATA_FRAME_LEN,
    FixedKeyMinuteIndexedRecordsArchiveWriter, IoReaderFactory, IoWriterFactory, L2_FRAME_LEN,
    MinuteCells, PrimaryKeyFixedSize, RECORD_LEN_PREFIX, RecordInsert, RecordPlace, RecordRef,
    RecordSplitInto, SPLIT_DATA_HEADER_LEN,
};

/// A record of a key the archive holds: its place in the key's chain is found, how it goes in is decided
/// with every frame it touches cached, and only then does anything change — with no await from the first
/// change to the last, so a write that fails or is dropped never leaves a record half moved.
impl<TKey, TFactory, TWriter, TReader>
    FixedKeyMinuteIndexedRecordsArchiveWriter<TKey, TFactory, TWriter, TReader>
where
    TKey: PrimaryKeyFixedSize,
    TFactory: IoWriterFactory<TWriter> + IoReaderFactory<TReader>,
    TWriter: ArchiveFileWriter,
    TReader: ArchiveFileReader,
{
    /// Inserts `record`, of `time` in `minute`, into the chain of the key whose L2 frame is at `l2_offset`.
    ///
    /// Panics when a data frame the insert needs would take the data file past its limit.
    pub(crate) async fn insert_record(
        &mut self,
        l2_offset: u32,
        minute: usize,
        time: i64,
        record: &[u8],
    ) -> Result<(), ArchiveError> {
        self.load_l2(l2_offset).await?;

        let place = self.find_place(l2_offset, minute, time).await?;
        let insert = self
            .plan_insert(place, RECORD_LEN_PREFIX + record.len())
            .await?;

        self.check_data_room(insert.new_frames());
        self.apply_insert(l2_offset, minute, insert, record);
        Ok(())
    }

    /// Caches the L2 frame at `offset`, evicting nothing. A frame that is not cached is appended already,
    /// so it is read back through a reader of the index.
    async fn load_l2(&mut self, offset: u32) -> Result<(), ArchiveError> {
        if self.index_pages.touch_l2(offset) {
            return Ok(());
        }

        let bytes = self
            .index
            .read_back(&self.factory, offset, L2_FRAME_LEN)
            .await?;
        let cells = MinuteCells::decode(&bytes, offset as u64)?;

        self.index_pages.insert_l2(offset, cells, false);
        Ok(())
    }

    /// §1.6 of the format: with a record in the minute, after the records of the minute no later than
    /// `time`; with none, before the first record of the nearest later minute that has one; with no such
    /// minute, at the end of the chain. The frames it walks are cached.
    async fn find_place(
        &mut self,
        l2_offset: u32,
        minute: usize,
        time: i64,
    ) -> Result<RecordPlace, ArchiveError> {
        let cells = self.cached_l2(l2_offset);

        if let Some(first) = cells.get(minute) {
            return self.walk_to_place(first, time).await;
        }

        if let Some((_, later)) = cells.first_from(minute + 1) {
            let frame = self.own_frame(later)?;
            self.data.load(&self.factory, frame).await?;
            self.check_record_start(frame, later.offset())?;

            return Ok(RecordPlace::Before {
                frame,
                at: later.offset(),
            });
        }

        let Some((_, earlier)) = cells.last_before(minute) else {
            return Err(ArchiveError::Corrupted(format!(
                "the L2 frame at {l2_offset} has no record: a key is written with its first one"
            )));
        };

        let mut frame = self.own_frame(earlier)?;
        let mut hops = 0;

        loop {
            self.data.load(&self.factory, frame).await?;

            match self.data.frame(frame).next() {
                0 => return Ok(RecordPlace::End { frame }),
                next => frame = self.hop(frame, next, &mut hops)?,
            }
        }
    }

    /// From the first record of a minute, past every record no later than `time`.
    async fn walk_to_place(
        &mut self,
        first: RecordRef,
        time: i64,
    ) -> Result<RecordPlace, ArchiveError> {
        let mut frame = self.own_frame(first)?;
        let mut at = first.offset();
        let mut hops = 0;

        self.data.load(&self.factory, frame).await?;
        self.check_record_start(frame, at)?;

        loop {
            let current = self.data.frame(frame);

            while at < current.end() {
                let (len, record_time) = current.record_at(at, frame)?;

                if record_time > time {
                    return Ok(RecordPlace::Before { frame, at });
                }

                at += len;
            }

            match current.next() {
                0 => return Ok(RecordPlace::End { frame }),
                next => {
                    frame = self.hop(frame, next, &mut hops)?;
                    at = SPLIT_DATA_HEADER_LEN;
                    self.data.load(&self.factory, frame).await?;
                }
            }
        }
    }

    /// §1.7 of the format, the crate's one overflow rule, decided with the frames it touches cached.
    async fn plan_insert(
        &mut self,
        place: RecordPlace,
        len: usize,
    ) -> Result<RecordInsert, ArchiveError> {
        let (frame, at) = match place {
            RecordPlace::End { frame } => {
                let current = self.data.frame(frame);

                return Ok(if current.free() >= len {
                    RecordInsert::Into {
                        frame,
                        at: current.end(),
                    }
                } else {
                    RecordInsert::NewLastFrame { after: frame }
                });
            }
            RecordPlace::Before { frame, at } => (frame, at),
        };

        let current = self.data.frame(frame);

        if current.free() >= len {
            return Ok(RecordInsert::Into { frame, at });
        }

        let Some(boundary) = current.halving_boundary(frame)? else {
            return Err(ArchiveError::Corrupted(format!(
                "the data frame at {frame} is full with one record"
            )));
        };

        let upper = current.end() - boundary + if at > boundary { len } else { 0 };
        let next = current.next();

        if next != 0 {
            let mut hops = 0;
            self.hop(frame, next, &mut hops)?;
            self.data.load(&self.factory, next).await?;

            if self.data.frame(next).free() >= upper {
                return Ok(RecordInsert::Split {
                    frame,
                    at,
                    boundary,
                    into: RecordSplitInto::Next { next },
                });
            }
        }

        Ok(RecordInsert::Split {
            frame,
            at,
            boundary,
            into: RecordSplitInto::NewFrame { next },
        })
    }

    /// Makes the change `insert` decided, moving the L2 cells with every record that moves. No await.
    fn apply_insert(&mut self, l2_offset: u32, minute: usize, insert: RecordInsert, record: &[u8]) {
        let mut prefixed = Vec::with_capacity(RECORD_LEN_PREFIX + record.len());
        prefixed.extend_from_slice(&(record.len() as u16).to_le_bytes());
        prefixed.extend_from_slice(record);
        let len = prefixed.len();

        let first_of_minute = self.cached_l2(l2_offset).get(minute);
        let becomes_first =
            |frame: u32, at: usize| first_of_minute.is_none_or(|first| first == here(frame, at));

        let (placed, moves) = match insert {
            RecordInsert::Into { frame, at } => {
                self.data.modify(frame).insert(at, &prefixed);

                let moves = CellMoves::Shift {
                    frame,
                    from: at,
                    by: len,
                };
                (becomes_first(frame, at).then(|| here(frame, at)), moves)
            }
            RecordInsert::NewLastFrame { after } => {
                let new = self.data.allocate();
                self.data
                    .modify(new)
                    .insert(SPLIT_DATA_HEADER_LEN, &prefixed);
                self.data.modify(after).set_next(new);

                let placed = first_of_minute
                    .is_none()
                    .then(|| here(new, SPLIT_DATA_HEADER_LEN));
                (placed, CellMoves::None)
            }
            RecordInsert::Split {
                frame,
                at,
                boundary,
                into,
            } => {
                let in_upper = at > boundary;
                let is_first = becomes_first(frame, at);

                let mut upper = self.data.modify(frame).split_off(boundary);

                if in_upper {
                    let within = at - boundary;
                    upper.splice(within..within, prefixed.iter().copied());
                }

                let (to, next) = match into {
                    RecordSplitInto::Next { next } => {
                        self.data.modify(next).insert(SPLIT_DATA_HEADER_LEN, &upper);
                        (next, Some(next))
                    }
                    RecordSplitInto::NewFrame { next } => {
                        let new = self.data.allocate();
                        let moved = self.data.modify(new);
                        moved.insert(SPLIT_DATA_HEADER_LEN, &upper);
                        moved.set_next(next);
                        self.data.modify(frame).set_next(new);
                        (new, None)
                    }
                };

                if !in_upper {
                    self.data.modify(frame).insert(at, &prefixed);
                }

                let placed = is_first.then(|| {
                    if in_upper {
                        here(to, SPLIT_DATA_HEADER_LEN + at - boundary)
                    } else {
                        here(frame, at)
                    }
                });

                let moves = CellMoves::Split {
                    frame,
                    at,
                    boundary,
                    len,
                    in_upper,
                    to,
                    next,
                    upper: upper.len(),
                };
                (placed, moves)
            }
        };

        let cells = self
            .index_pages
            .modify_l2(l2_offset)
            .expect("the L2 frame of a key being written is cached");

        cells.remap(|cell| moves.apply(cell));

        if let Some(placed) = placed {
            cells.set(minute, placed);
        }
    }

    fn cached_l2(&self, offset: u32) -> &MinuteCells {
        self.index_pages
            .l2(offset)
            .expect("the L2 frame of a key being written is cached")
    }

    /// The frame a cell of this writer's own index points at: always in data file 0.
    fn own_frame(&self, cell: RecordRef) -> Result<u32, ArchiveError> {
        if cell.file_no != 0 {
            return Err(ArchiveError::Corrupted(format!(
                "a cell points into data file {}; a working archive writes data file 0 only",
                cell.file_no
            )));
        }

        Ok(cell.frame)
    }

    /// A record must start where a cell says one does, inside the frame's records.
    fn check_record_start(&self, frame: u32, at: usize) -> Result<(), ArchiveError> {
        let current = self.data.frame(frame);

        if at < SPLIT_DATA_HEADER_LEN || at >= current.end() {
            return Err(ArchiveError::Corrupted(format!(
                "a cell points at {at} of the data frame at {frame}, whose records end at {}",
                current.end()
            )));
        }

        current.record_at(at, frame).map(|_| ())
    }

    /// The next frame of a chain. Every frame of the data file visited once at most: a chain longer than the
    /// file's frames runs in a cycle.
    fn hop(&self, frame: u32, next: u32, hops: &mut u64) -> Result<u32, ArchiveError> {
        *hops += 1;

        if next == frame || *hops > self.data.reserved() / DATA_FRAME_LEN as u64 {
            return Err(ArchiveError::Corrupted(format!(
                "the chain of data frames runs in a cycle at the frame at {frame}"
            )));
        }

        Ok(next)
    }
}

fn here(frame: u32, at: usize) -> RecordRef {
    RecordRef::new(0, frame, at)
}

/// Where the cells of the records an insert moved point to afterwards.
enum CellMoves {
    None,
    /// The records of `frame` from `from` on moved up by `by`.
    Shift {
        frame: u32,
        from: usize,
        by: usize,
    },
    /// `frame` was halved at `boundary`: the record of `len` went in at `at` of the lower half, or into the
    /// upper one when `in_upper`, and the upper half — `upper` bytes, the record included when it went there
    /// — now starts the records of `to`, in front of what `next` held when `to` is that next frame.
    Split {
        frame: u32,
        at: usize,
        boundary: usize,
        len: usize,
        in_upper: bool,
        to: u32,
        next: Option<u32>,
        upper: usize,
    },
}

impl CellMoves {
    /// Where a cell pointing at `cell` before the insert points after it.
    fn apply(&self, cell: RecordRef) -> RecordRef {
        match *self {
            Self::None => cell,
            Self::Shift { frame, from, by } => {
                if cell.frame == frame && cell.offset() >= from {
                    here(frame, cell.offset() + by)
                } else {
                    cell
                }
            }
            Self::Split {
                frame,
                at,
                boundary,
                len,
                in_upper,
                to,
                next,
                upper,
            } => {
                let offset = cell.offset();

                if cell.frame == frame {
                    if offset < boundary {
                        let moved = !in_upper && offset >= at;
                        here(frame, offset + if moved { len } else { 0 })
                    } else {
                        let behind_record = in_upper && offset >= at;
                        let within = offset - boundary + if behind_record { len } else { 0 };
                        here(to, SPLIT_DATA_HEADER_LEN + within)
                    }
                } else if Some(cell.frame) == next {
                    here(to, offset + upper)
                } else {
                    cell
                }
            }
        }
    }
}
