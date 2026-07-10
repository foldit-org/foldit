//! molex stores atom names as left-justified, null/space-padded 4-byte
//! buffers. Any consumer that displays or compares a name must first drop
//! that padding; this module owns the single trim kernel they share.

/// The significant bytes of a padded 4-byte atom name: everything up to the
/// first null or space. Names are left-justified, so trimming the trailing
/// padding is enough and no leading trim is needed.
pub fn trimmed_atom_name(raw: &[u8; 4]) -> &[u8] {
    let end = raw
        .iter()
        .position(|b| *b == 0 || *b == b' ')
        .unwrap_or(raw.len());
    &raw[..end]
}
