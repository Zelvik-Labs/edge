//! Metric key templates parsed once per immutable configuration generation.
use std::ops::Range;
use std::sync::Arc;

#[derive(Debug)]
enum Part {
    Literal(Range<usize>),
    Variable(Range<usize>),
}

#[derive(Debug)]
pub struct CompiledTemplate {
    source: Arc<str>,
    parts: Vec<Part>,
}

impl CompiledTemplate {
    pub fn compile(source: Arc<str>) -> Self {
        let mut parts = Vec::new();
        let mut cursor = 0;
        while let Some(relative_start) = source[cursor..].find("${") {
            let start = cursor + relative_start;
            if start > cursor {
                parts.push(Part::Literal(cursor..start));
            }
            let variable_start = start + 2;
            let Some(relative_end) = source[variable_start..].find('}') else {
                parts.push(Part::Literal(start..source.len()));
                cursor = source.len();
                break;
            };
            let end = variable_start + relative_end;
            parts.push(Part::Variable(variable_start..end));
            cursor = end + 1;
        }
        if cursor < source.len() {
            parts.push(Part::Literal(cursor..source.len()));
        }
        Self { source, parts }
    }

    pub fn variables(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(|part| match part {
            Part::Variable(range) => Some(&self.source[range.clone()]),
            Part::Literal(_) => None,
        })
    }

    pub fn render(&self, mut resolve: impl FnMut(&str) -> String) -> String {
        // A single variable is the common metric key. Return its owned value
        // directly instead of allocating a second output buffer and copying it.
        if let [part] = self.parts.as_slice() {
            return match part {
                Part::Variable(range) => resolve(&self.source[range.clone()]),
                Part::Literal(range) => self.source[range.clone()].to_string(),
            };
        }
        let mut out = String::with_capacity(self.source.len());
        for part in &self.parts {
            match part {
                Part::Literal(range) => out.push_str(&self.source[range.clone()]),
                Part::Variable(range) => out.push_str(&resolve(&self.source[range.clone()])),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The existing public resolver remains the reference. Kept separately
    // here so the standalone probe can test the parser without the full node.
    fn legacy(source: &str, mut resolve: impl FnMut(&str) -> String) -> String {
        let Some(mut cursor) = source.find("${") else {
            return source.to_string();
        };
        let mut out = String::with_capacity(source.len());
        out.push_str(&source[..cursor]);
        while cursor < source.len() {
            let variable_start = cursor + 2;
            let Some(relative_end) = source[variable_start..].find('}') else {
                out.push_str(&source[cursor..]);
                return out;
            };
            let variable_end = variable_start + relative_end;
            out.push_str(&resolve(&source[variable_start..variable_end]));
            let next = variable_end + 1;
            if let Some(relative) = source[next..].find("${") {
                cursor = next + relative;
                out.push_str(&source[next..cursor]);
            } else {
                out.push_str(&source[next..]);
                return out;
            }
        }
        out
    }

    #[test]
    fn compiled_rendering_and_variable_order_match_legacy_boundary_inputs() {
        let fragments = [
            "", "a", "中文", "$", "{", "}", "${x}", "${", "${}", "${ x }", "${${x}",
        ];
        for a in fragments {
            for b in fragments {
                for c in fragments {
                    let source = format!("{a}{b}{c}");
                    let compiled = CompiledTemplate::compile(Arc::from(source.as_str()));
                    for value in ["", "literal ${x}", "内容"] {
                        let mut variables = Vec::new();
                        let expected = legacy(&source, |name| {
                            variables.push(name.to_string());
                            value.to_string()
                        });
                        assert_eq!(
                            compiled.variables().collect::<Vec<_>>(),
                            variables,
                            "{source}"
                        );
                        assert_eq!(compiled.render(|_| value.to_string()), expected, "{source}");
                    }
                }
            }
        }
    }

    #[test]
    fn pure_variable_returns_the_resolvers_owned_buffer() {
        let compiled = CompiledTemplate::compile(Arc::from("${host}"));
        let resolved = String::from("long.example.test");
        let pointer = resolved.as_ptr();
        let mut value = Some(resolved);
        let result = compiled.render(|name| {
            assert_eq!(name, "host");
            value.take().unwrap()
        });
        assert_eq!(result.as_ptr(), pointer);
        assert!(value.is_none());
    }
}
