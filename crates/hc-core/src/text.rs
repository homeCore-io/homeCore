//! Small helpers for text that goes into logs and descriptions.

/// The start of `s`, at most `max` bytes, cut where a character begins.
///
/// `&s[..max]` panics when `max` falls inside a character, and rule text is
/// full of them: "35 °C", "—", an emoji. The rule engine runs on one task, so
/// one such panic stopped every rule until homeCore was restarted.
pub fn head(s: &str, max: usize) -> &str {
    &s[..s.floor_char_boundary(max)]
}

#[cfg(test)]
mod tests {
    use super::head;

    #[test]
    fn a_cut_inside_a_character_moves_back_to_its_start() {
        // "°" is two bytes; byte 4 falls between them.
        assert_eq!(head("21 °C", 4), "21 ");
        assert_eq!(head("Garage — check", 8), "Garage ");
        assert_eq!(head("short", 60), "short");
        assert_eq!(head("", 3), "");
    }
}
