//! Parsing `.drv` ATerm, the inverse of [`Derivation::unparse`].

use crate::derivation::{Derivation, InputNode, Output};
use crate::hash::{Algo, Hash};
use crate::path::{Method, StorePath};

#[derive(Debug, thiserror::Error)]
#[error("bad derivation at byte {pos}: {msg}")]
pub struct ParseError {
    pub pos: usize,
    pub msg: String,
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

type R<T> = Result<T, ParseError>;

impl<'a> P<'a> {
    fn err<T>(&self, msg: impl Into<String>) -> R<T> {
        Err(ParseError { pos: self.i, msg: msg.into() })
    }

    fn expect(&mut self, lit: &str) -> R<()> {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            Ok(())
        } else {
            self.err(format!("expected '{lit}'"))
        }
    }

    fn peek(&self, c: u8) -> bool {
        self.s.get(self.i) == Some(&c)
    }

    fn string(&mut self) -> R<String> {
        self.expect("\"")?;
        let mut out = Vec::new();
        loop {
            match self.s.get(self.i) {
                None => return self.err("unterminated string"),
                Some(b'"') => break,
                Some(b'\\') => {
                    self.i += 1;
                    out.push(match self.s.get(self.i) {
                        Some(b'n') => b'\n',
                        Some(b'r') => b'\r',
                        Some(b't') => b'\t',
                        Some(&c) => c,
                        None => return self.err("unterminated escape"),
                    });
                }
                Some(&c) => out.push(c),
            }
            self.i += 1;
        }
        self.i += 1;
        String::from_utf8(out).or_else(|_| self.err("not UTF-8"))
    }

    fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> R<T>) -> R<Vec<T>> {
        self.expect("[")?;
        let mut v = Vec::new();
        while !self.peek(b']') {
            if !v.is_empty() {
                self.expect(",")?;
            }
            v.push(item(self)?);
        }
        self.expect("]")?;
        Ok(v)
    }

    fn path(&mut self) -> R<StorePath> {
        let s = self.string()?;
        StorePath::parse(&s).or_else(|e| self.err(e.to_string()))
    }

    /// `,<outputs>` or `,(<outputs>,[(<output>,<node>)…])`, the leading comma included.
    fn node(&mut self) -> R<InputNode> {
        self.expect(",")?;
        if self.peek(b'(') {
            self.expect("(")?;
            let outputs = self.list(Self::string)?.into_iter().collect();
            self.expect(",")?;
            let children = self
                .list(|p| {
                    p.expect("(")?;
                    let k = p.string()?;
                    let n = p.node()?;
                    p.expect(")")?;
                    Ok((k, n))
                })?
                .into_iter()
                .collect();
            self.expect(")")?;
            Ok(InputNode { outputs, children })
        } else {
            Ok(InputNode { outputs: self.list(Self::string)?.into_iter().collect(), children: Default::default() })
        }
    }
}

fn parse_output(path: &str, method_algo: &str, hash: &str) -> Result<Output, String> {
    if method_algo.is_empty() {
        return if path.is_empty() {
            Ok(Output::Deferred)
        } else {
            StorePath::parse(path).map(Output::InputAddressed).map_err(|e| e.to_string())
        };
    }
    let (method, algo) = match method_algo.split_once(':') {
        Some(("r", a)) => (Method::Nar, a),
        Some(("text", a)) => (Method::Text, a),
        Some((m, _)) => return Err(format!("unknown ingestion method '{m}'")),
        None => (Method::Flat, method_algo),
    };
    let algo = Algo::parse(algo).ok_or(format!("unknown hash algorithm '{algo}'"))?;
    Ok(match hash {
        "" => Output::CaFloating { method, algo },
        "impure" => Output::Impure { method, algo },
        h => Output::CaFixed { method, hash: Hash::parse(h, Some(algo)).map_err(|e| e.to_string())? },
    })
}

/// Parse a `.drv` file. `name` is the derivation name (the file name minus hash and `.drv`).
pub fn parse(name: &str, s: &str) -> R<Derivation> {
    let mut p = P { s: s.as_bytes(), i: 0 };
    if p.peek(b'D') && s.starts_with("DrvWithVersion(") {
        p.expect("DrvWithVersion(")?;
        if p.string()? != "xp-dyn-drv" {
            return p.err("unknown derivation version");
        }
        p.expect(",")?;
    } else {
        p.expect("Derive(")?;
    }
    let mut drv = Derivation { name: name.to_string(), ..Default::default() };
    for (k, path, ma, h) in p.list(|p| {
        p.expect("(")?;
        let k = p.string()?;
        p.expect(",")?;
        let path = p.string()?;
        p.expect(",")?;
        let ma = p.string()?;
        p.expect(",")?;
        let h = p.string()?;
        p.expect(")")?;
        Ok((k, path, ma, h))
    })? {
        let out = parse_output(&path, &ma, &h).or_else(|e| p.err(e))?;
        drv.outputs.insert(k, out);
    }
    p.expect(",")?;
    for (path, n) in p.list(|p| {
        p.expect("(")?;
        let path = p.path()?;
        let n = p.node()?;
        p.expect(")")?;
        Ok((path, n))
    })? {
        drv.input_drvs.insert(path, n);
    }
    p.expect(",")?;
    drv.input_srcs = p.list(P::path)?.into_iter().collect();
    p.expect(",")?;
    drv.platform = p.string()?;
    p.expect(",")?;
    drv.builder = p.string()?;
    p.expect(",")?;
    drv.args = p.list(P::string)?;
    p.expect(",")?;
    for (k, v) in p.list(|p| {
        p.expect("(")?;
        let k = p.string()?;
        p.expect(",")?;
        let v = p.string()?;
        p.expect(")")?;
        Ok((k, v))
    })? {
        if k == "__json" {
            drv.structured_attrs = Some(v);
        } else {
            drv.env.insert(k, v);
        }
    }
    p.expect(")")?;
    if p.i != p.s.len() {
        return p.err("trailing data");
    }
    Ok(drv)
}
