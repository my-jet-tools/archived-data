/// Where a new record belongs in its key's chain of data frames, found before anything changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordPlace {
    /// Before the record at `at` of the frame at `frame`: the first record later than the new one.
    Before { frame: u32, at: usize },
    /// After every record, at the end of the chain's last frame.
    End { frame: u32 },
}

/// How a new record goes into its place — decided, with every frame it touches cached, before the first
/// change — by the one overflow rule of the crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordInsert {
    /// The frame has room: the records from `at` on move up by the record's length.
    Into { frame: u32, at: usize },
    /// The chain's last frame is full and the record goes after its last record: a new frame, chained
    /// after it, holds the record. Nothing moves.
    NewLastFrame { after: u32 },
    /// The frame is full and the record goes before one of its records: the frame is halved at the record
    /// `boundary` nearest its middle, the record goes into the half its place is in, and the upper half
    /// goes where `into` says.
    Split {
        frame: u32,
        at: usize,
        boundary: usize,
        into: RecordSplitInto,
    },
}

/// Where the upper half of a halved frame goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordSplitInto {
    /// In front of the records of the next frame of the chain, which has room for it.
    Next { next: u32 },
    /// Into a new frame chained between the halved frame and `next`, which has no room — or is 0, when the
    /// halved frame was the chain's last.
    NewFrame { next: u32 },
}

impl RecordInsert {
    /// How many new data frames the insert allocates.
    pub fn new_frames(&self) -> usize {
        match self {
            Self::Into { .. }
            | Self::Split {
                into: RecordSplitInto::Next { .. },
                ..
            } => 0,
            Self::NewLastFrame { .. }
            | Self::Split {
                into: RecordSplitInto::NewFrame { .. },
                ..
            } => 1,
        }
    }
}
