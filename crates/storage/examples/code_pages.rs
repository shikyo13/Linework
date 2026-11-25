//! Prints every legacy single-byte table this system provides, one line per
//! table: its name, then the 256 characters as hexadecimal code points (`-`
//! where a byte has none). Run on each system and compare the outputs to see
//! that a file opens to the same text everywhere.
fn main() {
    for &(_, label) in linework_storage::ENCODINGS {
        let Some(table) = linework_storage::code_page(label) else { continue };
        let cells: Vec<String> = table.iter().map(|c| c.map_or("-".into(), |c| format!("{:04X}", c as u32))).collect();
        println!("{label} {}", cells.join(" "));
    }
}
