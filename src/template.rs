//! Relative path templates such as `{name}/workspace`.
//!
//! Rules (see `docs/DESIGN.md`, "Templates"):
//!
//! - a relative path of non-empty `/`-separated components, none `.` or `..`;
//! - the only placeholder is `{name}`, which must appear at least once and may
//!   appear anywhere within components;
//! - any other `{` or `}` is an error;
//! - components and the interpolated path must respect `NAME_MAX` and
//!   `PATH_MAX`.

use std::fmt;

use crate::name::{MAX_NAME_LEN, Name};

/// Maximum length in bytes of an interpolated relative path, excluding the
/// terminating NUL (`PATH_MAX` is 4096 including it).
pub const MAX_PATH_LEN: usize = 4095;

const PLACEHOLDER: &str = "{name}";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Literal(String),
    Name,
}

/// A validated path template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    raw: String,
    components: Vec<Vec<Piece>>,
}

/// Why a template was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// The template is empty.
    #[error("template is empty")]
    Empty,
    /// The template begins with `/`.
    #[error("template must be relative (must not begin with '/')")]
    Absolute,
    /// The template ends with `/`.
    #[error("template must not end with '/'")]
    TrailingSlash,
    /// The template contains `//`.
    #[error("template contains an empty path component")]
    EmptyComponent,
    /// A component is `.` or `..`.
    #[error("template contains a '{0}' path component")]
    DotComponent(String),
    /// A `{` or `}` that is not part of `{name}`.
    #[error("template contains '{found}' at byte {offset}; the only placeholder is {{name}}")]
    InvalidBrace {
        /// The unexpected text, e.g. `{foo}` or a lone brace.
        found: String,
        /// Byte offset within the template.
        offset: usize,
    },
    /// The template contains a NUL byte.
    #[error("template contains a NUL byte")]
    Nul,
    /// The template does not contain `{name}`.
    #[error("template must contain {{name}}")]
    MissingName,
    /// A literal component is longer than `NAME_MAX`.
    #[error("template component is {0} bytes long; the maximum is {MAX_NAME_LEN}")]
    ComponentTooLong(usize),
}

/// Why interpolating a valid template with a valid name failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InterpolateError {
    /// An interpolated component exceeds `NAME_MAX`.
    #[error("interpolated path component is {0} bytes long; the maximum is {MAX_NAME_LEN}")]
    ComponentTooLong(usize),
    /// The interpolated path exceeds `PATH_MAX`.
    #[error("interpolated path is {0} bytes long; the maximum is {MAX_PATH_LEN}")]
    PathTooLong(usize),
}

impl Template {
    /// Parses and validates a template.
    pub fn parse(raw: &str) -> Result<Self, TemplateError> {
        if raw.is_empty() {
            return Err(TemplateError::Empty);
        }
        if raw.contains('\0') {
            return Err(TemplateError::Nul);
        }
        if raw.starts_with('/') {
            return Err(TemplateError::Absolute);
        }
        if raw.ends_with('/') {
            return Err(TemplateError::TrailingSlash);
        }

        let mut components = Vec::new();
        let mut has_name = false;
        let mut offset = 0;
        for component in raw.split('/') {
            if component.is_empty() {
                return Err(TemplateError::EmptyComponent);
            }
            if component == "." || component == ".." {
                return Err(TemplateError::DotComponent(component.to_owned()));
            }
            let pieces = parse_component(component, offset)?;
            has_name |= pieces.contains(&Piece::Name);
            if !pieces.contains(&Piece::Name) && component.len() > MAX_NAME_LEN {
                return Err(TemplateError::ComponentTooLong(component.len()));
            }
            components.push(pieces);
            offset += component.len() + 1;
        }
        if !has_name {
            return Err(TemplateError::MissingName);
        }
        Ok(Self {
            raw: raw.to_owned(),
            components,
        })
    }

    /// Returns the template as written in configuration.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Interpolates `name`, returning the path components.
    pub fn components(&self, name: &Name) -> Result<Vec<String>, InterpolateError> {
        let mut out = Vec::with_capacity(self.components.len());
        let mut total = 0;
        for pieces in &self.components {
            let mut component = String::new();
            for piece in pieces {
                match piece {
                    Piece::Literal(s) => component.push_str(s),
                    Piece::Name => component.push_str(name.as_str()),
                }
            }
            if component.len() > MAX_NAME_LEN {
                return Err(InterpolateError::ComponentTooLong(component.len()));
            }
            // Defense in depth: a valid name can never produce these.
            debug_assert!(component != "." && component != ".." && !component.contains('/'));
            total += component.len() + usize::from(!out.is_empty());
            out.push(component);
        }
        if total > MAX_PATH_LEN {
            return Err(InterpolateError::PathTooLong(total));
        }
        Ok(out)
    }

    /// Interpolates `name`, returning the relative path joined with `/`.
    pub fn interpolate(&self, name: &Name) -> Result<String, InterpolateError> {
        Ok(self.components(name)?.join("/"))
    }
}

fn parse_component(component: &str, base_offset: usize) -> Result<Vec<Piece>, TemplateError> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = component;
    let mut offset = base_offset;
    while let Some(pos) = rest.find(['{', '}']) {
        literal.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        if let Some(after) = tail.strip_prefix(PLACEHOLDER) {
            if !literal.is_empty() {
                pieces.push(Piece::Literal(std::mem::take(&mut literal)));
            }
            pieces.push(Piece::Name);
            rest = after;
            offset += pos + PLACEHOLDER.len();
        } else {
            let found = if tail.starts_with('{') {
                tail.find('}').map_or("{", |end| &tail[..=end])
            } else {
                "}"
            };
            return Err(TemplateError::InvalidBrace {
                found: found.to_owned(),
                offset: offset + pos,
            });
        }
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        pieces.push(Piece::Literal(literal));
    }
    Ok(pieces)
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    #[test]
    fn simple_templates() {
        let t = Template::parse("{name}/workspace").unwrap();
        assert_eq!(
            t.interpolate(&name("libcurl")).unwrap(),
            "libcurl/workspace"
        );
        assert_eq!(t.as_str(), "{name}/workspace");

        let t = Template::parse("{name}").unwrap();
        assert_eq!(t.interpolate(&name("x")).unwrap(), "x");
    }

    #[test]
    fn multiple_and_embedded_placeholders() {
        let t = Template::parse("pre-{name}-post/{name}/{name}{name}").unwrap();
        assert_eq!(
            t.components(&name("ab")).unwrap(),
            vec!["pre-ab-post", "ab", "abab"]
        );
    }

    #[test]
    fn literals_may_contain_dots_and_unicode() {
        let t = Template::parse("..hidden/{name}/.git/é").unwrap();
        assert_eq!(t.interpolate(&name("n")).unwrap(), "..hidden/n/.git/é");
    }

    #[test]
    fn rejects_structural_errors() {
        assert_eq!(Template::parse(""), Err(TemplateError::Empty));
        assert_eq!(Template::parse("/{name}"), Err(TemplateError::Absolute));
        assert_eq!(
            Template::parse("{name}/"),
            Err(TemplateError::TrailingSlash)
        );
        assert_eq!(
            Template::parse("a//{name}"),
            Err(TemplateError::EmptyComponent)
        );
        assert_eq!(
            Template::parse("../{name}"),
            Err(TemplateError::DotComponent("..".into()))
        );
        assert_eq!(
            Template::parse("{name}/."),
            Err(TemplateError::DotComponent(".".into()))
        );
        assert_eq!(Template::parse("a\0/{name}"), Err(TemplateError::Nul));
        assert_eq!(
            Template::parse("workspace"),
            Err(TemplateError::MissingName)
        );
    }

    #[test]
    fn rejects_invalid_braces() {
        let cases = [
            ("{foo}/{name}", "{foo}", 0),
            ("{name}/{NAME}", "{NAME}", 7),
            ("a{/{name}", "{", 1),
            ("{name}}", "}", 6),
            ("x/}{name}", "}", 2),
            ("{name/x", "{", 0),
            ("{{name}}", "{{name}", 0),
        ];
        for (input, found, offset) in cases {
            assert_eq!(
                Template::parse(input),
                Err(TemplateError::InvalidBrace {
                    found: found.into(),
                    offset
                }),
                "{input}"
            );
        }
    }

    #[test]
    fn literal_component_length() {
        let long = "a".repeat(MAX_NAME_LEN + 1);
        assert_eq!(
            Template::parse(&format!("{long}/{{name}}")),
            Err(TemplateError::ComponentTooLong(MAX_NAME_LEN + 1))
        );
        let ok = "a".repeat(MAX_NAME_LEN);
        assert!(Template::parse(&format!("{ok}/{{name}}")).is_ok());
    }

    #[test]
    fn interpolated_component_length() {
        let t = Template::parse("prefix-{name}").unwrap();
        let n = name(&"a".repeat(MAX_NAME_LEN));
        assert_eq!(
            t.components(&n),
            Err(InterpolateError::ComponentTooLong(MAX_NAME_LEN + 7))
        );
    }

    #[test]
    fn interpolated_path_length() {
        // 16 components of a 255-byte name plus 15 separators = 4095: allowed.
        let raw = vec!["{name}"; 16].join("/");
        let t = Template::parse(&raw).unwrap();
        let n = name(&"a".repeat(MAX_NAME_LEN));
        assert_eq!(t.interpolate(&n).unwrap().len(), MAX_PATH_LEN);

        let raw = format!("{raw}/x{{name}}");
        let t = Template::parse(&raw).unwrap();
        let n = name(&"a".repeat(MAX_NAME_LEN - 1));
        assert!(matches!(
            t.components(&n),
            Err(InterpolateError::PathTooLong(_))
        ));
    }

    #[test]
    fn valid_names_never_introduce_traversal() {
        let t = Template::parse("{name}/{name}.d/x{name}").unwrap();
        for s in ["a", "a.b", "a..b", "-", "_", "a-", "x.."] {
            for c in t.components(&name(s)).unwrap() {
                assert!(c != "." && c != ".." && !c.contains('/') && !c.is_empty());
            }
        }
    }
}
