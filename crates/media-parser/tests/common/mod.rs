use std::path::PathBuf;

pub(crate) fn fixtures_dir() -> PathBuf {
   PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join("fixtures")
}
