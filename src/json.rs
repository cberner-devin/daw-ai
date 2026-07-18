use std::collections::HashMap;
use std::fmt;

#[derive(Debug, PartialEq)]
pub(crate) struct JsonError(String);

impl JsonError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for JsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Self>),
    Object(HashMap<String, Self>),
}

impl JsonValue {
    pub(crate) fn as_object(&self) -> Option<&HashMap<String, Self>> {
        if let Self::Object(value) = self {
            Some(value)
        } else {
            None
        }
    }

    pub(crate) fn as_array(&self) -> Option<&[Self]> {
        if let Self::Array(value) = self {
            Some(value)
        } else {
            None
        }
    }

    pub(crate) fn as_string(&self) -> Option<&str> {
        if let Self::String(value) = self {
            Some(value)
        } else {
            None
        }
    }

    pub(crate) const fn as_number(&self) -> Option<f64> {
        if let Self::Number(value) = self {
            Some(*value)
        } else {
            None
        }
    }

    pub(crate) const fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(value) = self {
            Some(*value)
        } else {
            None
        }
    }

    pub(crate) fn to_json(&self) -> String {
        match self {
            Self::Null => "null".to_owned(),
            Self::Bool(value) => value.to_string(),
            Self::Number(value) => value.to_string(),
            Self::String(value) => crate::model::json_string(value),
            Self::Array(values) => format!(
                "[{}]",
                values
                    .iter()
                    .map(Self::to_json)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Self::Object(values) => {
                let mut entries = values.iter().collect::<Vec<_>>();
                entries.sort_by(|(left, _), (right, _)| left.cmp(right));
                format!(
                    "{{{}}}",
                    entries
                        .into_iter()
                        .map(|(key, value)| {
                            format!("{}:{}", crate::model::json_string(key), value.to_json())
                        })
                        .collect::<Vec<_>>()
                        .join(",")
                )
            }
        }
    }
}

pub(crate) struct JsonParser<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl<'a> JsonParser<'a> {
    pub(crate) const fn new(source: &'a str) -> Self {
        Self {
            bytes: source.as_bytes(),
            index: 0,
        }
    }

    pub(crate) fn parse(mut self) -> Result<JsonValue, JsonError> {
        let value = self.parse_value()?;
        self.skip_whitespace();
        if self.index != self.bytes.len() {
            return Err(JsonError::new("unexpected content after JSON value"));
        }
        Ok(value)
    }

    fn parse_value(&mut self) -> Result<JsonValue, JsonError> {
        self.skip_whitespace();
        match self.bytes.get(self.index) {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b't') => self.parse_literal(b"true", JsonValue::Bool(true)),
            Some(b'f') => self.parse_literal(b"false", JsonValue::Bool(false)),
            Some(b'n') => self.parse_literal(b"null", JsonValue::Null),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            _ => Err(JsonError::new("invalid JSON value")),
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, JsonError> {
        self.index += 1;
        let mut object = HashMap::new();
        loop {
            self.skip_whitespace();
            if self.consume(b'}') {
                return Ok(JsonValue::Object(object));
            }
            let key = self.parse_string()?;
            self.skip_whitespace();
            if !self.consume(b':') {
                return Err(JsonError::new("missing colon in JSON object"));
            }
            let value = self.parse_value()?;
            object.insert(key, value);
            self.skip_whitespace();
            if self.consume(b'}') {
                return Ok(JsonValue::Object(object));
            }
            if !self.consume(b',') {
                return Err(JsonError::new("missing comma in JSON object"));
            }
        }
    }

    fn parse_array(&mut self) -> Result<JsonValue, JsonError> {
        self.index += 1;
        let mut values = Vec::new();
        loop {
            self.skip_whitespace();
            if self.consume(b']') {
                return Ok(JsonValue::Array(values));
            }
            values.push(self.parse_value()?);
            self.skip_whitespace();
            if self.consume(b']') {
                return Ok(JsonValue::Array(values));
            }
            if !self.consume(b',') {
                return Err(JsonError::new("missing comma in JSON array"));
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        if !self.consume(b'"') {
            return Err(JsonError::new("expected JSON string"));
        }
        let mut output = Vec::new();
        while let Some(byte) = self.bytes.get(self.index).copied() {
            self.index += 1;
            match byte {
                b'"' => {
                    return String::from_utf8(output)
                        .map_err(|_| JsonError::new("JSON string was not valid UTF-8"));
                }
                b'\\' => self.parse_escape(&mut output)?,
                0..=31 => return Err(JsonError::new("control character in JSON string")),
                _ => output.push(byte),
            }
        }
        Err(JsonError::new("unterminated JSON string"))
    }

    fn parse_escape(&mut self, output: &mut Vec<u8>) -> Result<(), JsonError> {
        let Some(escaped) = self.bytes.get(self.index).copied() else {
            return Err(JsonError::new("unterminated JSON escape"));
        };
        self.index += 1;
        match escaped {
            b'"' | b'\\' | b'/' => output.push(escaped),
            b'b' => output.push(8),
            b'f' => output.push(12),
            b'n' => output.push(b'\n'),
            b'r' => output.push(b'\r'),
            b't' => output.push(b'\t'),
            b'u' => {
                let character = self.parse_unicode_escape()?;
                let mut bytes = [0_u8; 4];
                output.extend_from_slice(character.encode_utf8(&mut bytes).as_bytes());
            }
            _ => return Err(JsonError::new("invalid JSON escape")),
        }
        Ok(())
    }

    fn parse_unicode_escape(&mut self) -> Result<char, JsonError> {
        let first = self.parse_hex_codepoint()?;
        let codepoint = match first {
            0xD800..=0xDBFF => {
                if !self.consume(b'\\') || !self.consume(b'u') {
                    return Err(JsonError::new("unpaired high surrogate in JSON string"));
                }
                let second = self.parse_hex_codepoint()?;
                if !(0xDC00..=0xDFFF).contains(&second) {
                    return Err(JsonError::new("unpaired high surrogate in JSON string"));
                }
                0x1_0000 + ((first - 0xD800) << 10) + (second - 0xDC00)
            }
            0xDC00..=0xDFFF => {
                return Err(JsonError::new("unpaired low surrogate in JSON string"));
            }
            _ => first,
        };
        char::from_u32(codepoint)
            .ok_or_else(|| JsonError::new("invalid Unicode escape in JSON string"))
    }

    fn parse_hex_codepoint(&mut self) -> Result<u32, JsonError> {
        if self.index + 4 > self.bytes.len() {
            return Err(JsonError::new("short Unicode escape in JSON string"));
        }
        let mut value = 0_u32;
        for byte in &self.bytes[self.index..self.index + 4] {
            value = value * 16
                + match byte {
                    b'0'..=b'9' => u32::from(byte - b'0'),
                    b'a'..=b'f' => u32::from(byte - b'a' + 10),
                    b'A'..=b'F' => u32::from(byte - b'A' + 10),
                    _ => return Err(JsonError::new("invalid Unicode escape in JSON string")),
                };
        }
        self.index += 4;
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<JsonValue, JsonError> {
        let start = self.index;
        while matches!(
            self.bytes.get(self.index),
            Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        ) {
            self.index += 1;
        }
        let number = std::str::from_utf8(&self.bytes[start..self.index])
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .ok_or_else(|| JsonError::new("invalid JSON number"))?;
        Ok(JsonValue::Number(number))
    }

    fn parse_literal(&mut self, literal: &[u8], value: JsonValue) -> Result<JsonValue, JsonError> {
        if self.bytes.get(self.index..self.index + literal.len()) == Some(literal) {
            self.index += literal.len();
            Ok(value)
        } else {
            Err(JsonError::new("invalid JSON literal"))
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(
            self.bytes.get(self.index),
            Some(b' ' | b'\n' | b'\r' | b'\t')
        ) {
            self.index += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.index) == Some(&byte) {
            self.index += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_renders_nested_json() {
        let source = r#"{"text":"sparkle \uD83C\uDFB6","items":[true,null,1.5]}"#;
        let value = JsonParser::new(source).parse().expect("valid JSON");
        let rendered = value.to_json();
        let reparsed = JsonParser::new(&rendered).parse().expect("rendered JSON");
        assert_eq!(
            reparsed.as_object().unwrap()["text"].as_string(),
            Some("sparkle \u{1F3B6}")
        );
    }

    #[test]
    fn rejects_trailing_content_and_unpaired_surrogates() {
        assert!(JsonParser::new("{} no").parse().is_err());
        assert!(JsonParser::new(r#""\uD83C""#).parse().is_err());
        assert!(JsonParser::new(r#""\uDFB6""#).parse().is_err());
    }
}
