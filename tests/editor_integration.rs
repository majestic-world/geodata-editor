use std::{
    env, fs,
    time::{SystemTime, UNIX_EPOCH},
};

use geodata_editor::l2j::{Document, LayerAddress};

#[test]
#[ignore = "requires GEODATA_EDITOR_L2J pointing to a local real L2J"]
fn opens_edits_saves_and_revalidates_a_real_l2j() {
    let source = env::var("GEODATA_EDITOR_L2J").expect("set GEODATA_EDITOR_L2J");
    let source_bytes = fs::read(&source).expect("read source L2J");
    let mut document = Document::open(&source).expect("open source L2J");
    document.set_nswe([LayerAddress::new(0, 0, 0)], 0, "integration edit");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let output = env::temp_dir().join(format!("geodata-editor-{nonce}.l2j"));
    document.save_as(&output).expect("save edited L2J");
    Document::open(&output).expect("audit edited L2J");
    assert_eq!(
        fs::read(&source).expect("read original again"),
        source_bytes
    );
    fs::remove_file(output).expect("remove integration output");
}
