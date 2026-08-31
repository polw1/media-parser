use serde::{Serialize, Serializer};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
   #[error("{0}")]
   Custom(String),
   #[error("Media parser error: {0}")]
   MediaParser(#[from] media_parser::MediaParserError),
}

impl Serialize for Error {
   fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
   where
      S: Serializer,
   {
      serializer.serialize_str(self.to_string().as_ref())
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn media_parser_resource_limit_serialization_preserves_reason_and_guidance() {
      let error = Error::MediaParser(media_parser::MediaParserError::Other(
         "too many sample read batches; select by track ID or language, or request a narrower time range"
            .to_owned(),
      ));

      assert_eq!(
         serde_json::to_value(error).expect("the plugin error should serialize"),
         serde_json::json!(
            "Media parser error: Other error: too many sample read batches; select by track ID or language, or request a narrower time range"
         )
      );
   }
}
