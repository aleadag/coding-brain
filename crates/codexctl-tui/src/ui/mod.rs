// `brain` (full-screen Brain Review surface) stays in the binary crate as
// `src/brain_screen.rs` — it depends on `brain::metrics` and `brain::risk`
// which are binary-only modules. main.rs calls it directly.
pub mod brain;
pub mod detail;
pub mod help;
pub mod skills;
pub mod status_bar;
pub mod table;

#[cfg(test)]
mod tests {
    #[test]
    fn cost_labels_are_compact_and_explicit() {
        assert_eq!(super::table::TABLE_HEADERS[4], "Est. $");
        assert_eq!(super::detail::DETAIL_COST_TITLE, " Estimated cost");
        assert_eq!(crate::app::SORT_COLUMNS[2], "Cost");
    }
}
