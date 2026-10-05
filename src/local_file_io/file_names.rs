/// The name of an archive's index inside its directory: `index`.
pub fn index_file_name() -> &'static str {
    "index"
}

/// The name of data file `no` inside its directory: `data` and two lowercase hex digits, `data00` … `dataff`.
pub fn data_file_name(no: u8) -> String {
    format!("data{no:02x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_and_every_data_file_have_a_name_of_their_own() {
        assert_eq!(index_file_name(), "index");
        assert_eq!(data_file_name(0), "data00");
        assert_eq!(data_file_name(10), "data0a");
        assert_eq!(data_file_name(171), "dataab");
        assert_eq!(data_file_name(255), "dataff");

        let mut names: Vec<String> = (0..=255).map(data_file_name).collect();
        names.push(index_file_name().to_string());
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 257);
    }
}
