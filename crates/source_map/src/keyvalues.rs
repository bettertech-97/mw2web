//! Valve KeyValues as Hammer writes them: `name { "key" "value" child { … } }`.

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Block {
    pub name: String,
    pub pairs: Vec<(String, String)>,
    pub children: Vec<Block>,
}

impl Block {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Block> + 'a {
        self.children
            .iter()
            .filter(move |child| child.name.eq_ignore_ascii_case(name))
    }

    pub fn child(&self, name: &str) -> Option<&Block> {
        self.children
            .iter()
            .find(|child| child.name.eq_ignore_ascii_case(name))
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token<'a> {
    Open,
    Close,
    Text(&'a str),
}

struct Lexer<'a> {
    text: &'a str,
    at: usize,
    line: usize,
}

impl<'a> Lexer<'a> {
    fn next(&mut self) -> Result<Option<Token<'a>>, String> {
        let bytes = self.text.as_bytes();
        loop {
            while self.at < bytes.len() && bytes[self.at].is_ascii_whitespace() {
                if bytes[self.at] == b'\n' {
                    self.line += 1;
                }
                self.at += 1;
            }
            if self.at >= bytes.len() {
                return Ok(None);
            }
            if bytes[self.at] == b'/' && bytes.get(self.at + 1) == Some(&b'/') {
                while self.at < bytes.len() && bytes[self.at] != b'\n' {
                    self.at += 1;
                }
                continue;
            }
            break;
        }
        match bytes[self.at] {
            b'{' => {
                self.at += 1;
                Ok(Some(Token::Open))
            }
            b'}' => {
                self.at += 1;
                Ok(Some(Token::Close))
            }
            b'"' => {
                let start = self.at + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end] != b'"' {
                    if bytes[end] == b'\n' {
                        self.line += 1;
                    }
                    end += 1;
                }
                if end >= bytes.len() {
                    return Err(format!("line {}: unterminated string", self.line));
                }
                self.at = end + 1;
                Ok(Some(Token::Text(&self.text[start..end])))
            }
            _ => {
                let start = self.at;
                while self.at < bytes.len()
                    && !bytes[self.at].is_ascii_whitespace()
                    && !matches!(bytes[self.at], b'{' | b'}' | b'"')
                {
                    self.at += 1;
                }
                Ok(Some(Token::Text(&self.text[start..self.at])))
            }
        }
    }
}

/// Every top-level block of the file, in order.
pub fn parse(text: &str) -> Result<Vec<Block>, String> {
    let mut lexer = Lexer {
        text,
        at: 0,
        line: 1,
    };
    let mut stack: Vec<Block> = Vec::new();
    let mut roots = Vec::new();
    let mut pending: Option<&str> = None;
    while let Some(token) = lexer.next()? {
        match token {
            Token::Open => {
                let name = pending
                    .take()
                    .ok_or_else(|| format!("line {}: `{{` without a block name", lexer.line))?;
                stack.push(Block {
                    name: name.to_owned(),
                    ..Block::default()
                });
            }
            Token::Close => {
                if pending.take().is_some() {
                    return Err(format!("line {}: key without a value", lexer.line));
                }
                let block = stack
                    .pop()
                    .ok_or_else(|| format!("line {}: `}}` without a block", lexer.line))?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(block),
                    None => roots.push(block),
                }
            }
            Token::Text(text) => match pending.take() {
                Some(key) => match stack.last_mut() {
                    Some(block) => block.pairs.push((key.to_owned(), text.to_owned())),
                    None => {
                        return Err(format!(
                            "line {}: `{key}` `{text}` outside of any block",
                            lexer.line
                        ));
                    }
                },
                None => pending = Some(text),
            },
        }
    }
    if !stack.is_empty() {
        return Err(format!("unclosed block `{}`", stack.last().map_or("", |b| &b.name)));
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_blocks_and_pairs() {
        let text = r#"
world
{
	"id" "1"
	"classname" "worldspawn"
	solid
	{
		"id" "2"
		side
		{
			"plane" "(0 0 0) (0 1 0) (1 1 0)"
			"material" "tools/toolsnodraw"
		}
	}
}
entity
{
	"classname" "info_player_terrorist"
	"origin" "1 2 3"
}
"#;
        let roots = parse(text).unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].name, "world");
        assert_eq!(roots[0].get("classname"), Some("worldspawn"));
        let solid = roots[0].child("solid").unwrap();
        let side = solid.child("side").unwrap();
        assert_eq!(side.get("material"), Some("tools/toolsnodraw"));
        assert_eq!(roots[1].get("origin"), Some("1 2 3"));
    }
}
