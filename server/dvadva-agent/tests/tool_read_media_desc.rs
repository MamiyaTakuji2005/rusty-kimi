mod tool_test_utils;

use std::collections::HashSet;

use dvadva_agent::config::ModelCapability;
use dvadva_agent::tools::SkipThisTool;
use dvadva_agent::tools::file::ReadMediaFile;
use kosong::tooling::CallableTool2;

use tool_test_utils::RuntimeFixture;

/// Everything above the capability line, which is the same whatever the model
/// can take. Kept separate so a wording change touches one place.
const BODY: &str = "\
Read an image or video file as content you can look at directly.\n\
\n\
- Reads at most 100MB; a larger file is rejected.\n\
- Text files are rejected — use ReadFile for those.\n";

fn description_for(capabilities: &[ModelCapability]) -> String {
    let caps: HashSet<ModelCapability> = capabilities.iter().cloned().collect();
    let fixture = RuntimeFixture::with_capabilities(caps);
    let tool = ReadMediaFile::new(&fixture.runtime).expect("read media tool");
    // Normalize away checkout-dependent CRLF in the embedded markdown.
    tool.description().to_string().replace("\r\n", "\n")
}

#[test]
fn test_read_media_file_description_by_capabilities() {
    assert_eq!(
        description_for(&[ModelCapability::ImageIn, ModelCapability::VideoIn]),
        format!("{BODY}- The current model accepts both images and video.\n")
    );
    assert_eq!(
        description_for(&[ModelCapability::ImageIn]),
        format!("{BODY}- The current model accepts images but not video.\n")
    );
    assert_eq!(
        description_for(&[ModelCapability::VideoIn]),
        format!("{BODY}- The current model accepts video but not images.\n")
    );
}

#[test]
fn test_read_media_file_description_without_capabilities() {
    let caps = HashSet::new();
    let fixture = RuntimeFixture::with_capabilities(caps);
    let result = ReadMediaFile::new(&fixture.runtime);
    assert!(matches!(result, Err(SkipThisTool)));
}
