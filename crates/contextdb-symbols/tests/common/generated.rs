//! Source renderer and semantic model: no tree-sitter, no normalised hashes.
use contextdb_anchors::Language;
use std::collections::BTreeMap;

pub const LANGS: [(Language, &str); 6] = [
    (Language::Rust, "src/a.rs"),
    (Language::TypeScript, "src/b.ts"),
    (Language::Tsx, "src/c.tsx"),
    (Language::JavaScript, "src/d.js"),
    (Language::Python, "src/e.py"),
    (Language::Go, "src/f.go"),
];
pub const STEPS: usize = 20;
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Meaning {
    pub name: String,
    pub body: u64,
    pub signature: bool,
    pub attribute: bool,
    pub directive: bool,
}
#[derive(Clone)]
pub struct Host {
    pub functions: Vec<Meaning>,
    pub pretty: bool,
    pub comment: u64,
    pub bad: bool,
    pub missing: bool,
}
impl Host {
    pub fn meanings(&self) -> BTreeMap<String, Vec<Meaning>> {
        let mut out: BTreeMap<String, Vec<Meaning>> = BTreeMap::new();
        if !self.missing {
            for f in &self.functions {
                out.entry(f.name.clone()).or_default().push(f.clone());
            }
        }
        out
    }
    pub fn render(&self, l: Language) -> Option<Vec<u8>> {
        if self.missing {
            return None;
        }
        let mut out = if l == Language::Go {
            "package p\n".to_owned()
        } else {
            String::new()
        };
        for (index, f) in self.functions.iter().enumerate() {
            let name = &f.name;
            let n = f.body;
            let t = if f.signature { "str" } else { "int" };
            let ty = if f.signature { "u32" } else { "u8" };
            let comment = self.comment;
            let expression = if self.bad && index == 0 {
                "x +* ".to_owned()
            } else {
                format!("x + {n}")
            };
            match l {
                Language::Rust => {
                    out.push_str(if f.attribute {
                        "#[cold]\n"
                    } else {
                        "#[inline]\n"
                    });
                    let gap = if self.pretty { "\n    " } else { " " };
                    out.push_str(&format!("// ordinary {comment}\nfn {name}(x: {ty}) -> {ty} {{{gap}return {expression};\n}}\n"));
                }
                Language::Python => {
                    out.push_str(if f.attribute { "@other\n" } else { "@dec\n" });
                    let annotation = if f.directive { "str" } else { "int" };
                    let param = if self.pretty {
                        format!("\n    x: {t},\n")
                    } else {
                        format!("x: {t}")
                    };
                    out.push_str(&format!("def {name}({param}):\n    # ordinary {comment}\n    y = x  # type: {annotation}\n    return {expression}\n"));
                }
                Language::Go => {
                    out.push_str(if f.directive {
                        "//go:nosplit\n"
                    } else {
                        "//go:noinline\n"
                    });
                    let t = if f.signature { "uint" } else { "int" };
                    let gap = if self.pretty { "\n\t" } else { " " };
                    out.push_str(&format!("// ordinary {comment}\nfunc {name}(x {t}) {t} {{{gap}return {expression}\n}}\n"));
                }
                _ => {
                    out.push_str(if f.directive {
                        "// @ts-expect-error\n"
                    } else {
                        "// @ts-ignore\n"
                    });
                    let param = if l == Language::JavaScript {
                        if f.signature { "x, y" } else { "x" }
                    } else if f.signature {
                        "x: string"
                    } else {
                        "x: number"
                    };
                    let gap = if self.pretty { "\n    " } else { " " };
                    out.push_str(&format!("// ordinary {comment}\nfunction {name}({param}) {{{gap}return {expression};\n}}\n"));
                }
            }
        }
        Some(out.into_bytes())
    }
}
#[derive(Clone)]
pub struct Step {
    pub hosts: Vec<Host>,
    pub event: &'static str,
    pub target: usize,
}
impl Step {
    pub fn world(&self) -> BTreeMap<String, Vec<u8>> {
        LANGS
            .iter()
            .zip(&self.hosts)
            .filter_map(|((l, path), host)| host.render(*l).map(|b| ((*path).to_owned(), b)))
            .collect()
    }
}
pub fn plan(seed: u64) -> Vec<Step> {
    // Fixed seeded choices change language, names, values and which changes
    // overlap. Event schedule is explicit so every floor is reproducible.
    let mixed = seed.wrapping_mul(0x9e3779b97f4a7c15).rotate_left(17);
    let offset = (mixed % 6) as usize;
    let suffix = (mixed >> 12) % 16;
    let initial = Host {
        functions: [
            format!("f{suffix}"),
            format!("F{suffix}"),
            format!("g{suffix}"),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, name)| Meaning {
            name,
            body: 1 + i as u64 + (mixed >> 24) % 5,
            signature: false,
            attribute: false,
            directive: false,
        })
        .collect(),
        pretty: false,
        comment: 0,
        bad: false,
        missing: false,
    };
    let mut hosts = vec![initial.clone(); 6];
    let mut steps = vec![Step {
        hosts: hosts.clone(),
        event: "insert",
        target: offset,
    }];
    let deleted = (offset + 3) % 6;
    let error = (offset + 4) % 6;
    let mut saved = None;
    for k in 1..STEPS {
        let mut target = (offset + k - 1) % 6;
        let event = match k {
            1 | 15 => {
                hosts[target].functions[0].body += 10;
                "body"
            }
            2 => {
                hosts[target].functions[0].signature = true;
                "signature"
            }
            3 | 16 => {
                hosts[target].pretty = !hosts[target].pretty;
                "format"
            }
            4 | 17 => {
                hosts[target].comment += 1;
                "comment"
            }
            5 => {
                target = [1, 2, 3, 4, 5][offset % 5];
                hosts[target].functions[0].directive = true;
                "directive"
            }
            6 => {
                target = if offset.is_multiple_of(2) { 0 } else { 4 };
                hosts[target].functions[0].attribute = true;
                "attribute"
            }
            7 => {
                hosts[target].functions[0].name.push_str("_new");
                "rename"
            }
            8 => {
                let mut f = hosts[target].functions[0].clone();
                f.body += 30;
                hosts[target].functions.push(f);
                "group"
            }
            9 => {
                target = deleted;
                hosts[target].missing = true;
                "delete"
            }
            10 => {
                target = deleted;
                hosts[target].missing = false;
                "restore"
            }
            11 => {
                target = offset;
                hosts[target] = initial.clone();
                "aba"
            }
            12 | 18 => {
                hosts[target].functions[2].body += 20;
                "outside"
            }
            13 => {
                target = error;
                saved = Some(hosts[target].clone());
                hosts[target].bad = true;
                "syntax"
            }
            14 => {
                target = error;
                hosts[target] = saved.take().unwrap();
                "restore"
            }
            19 => {
                let mut f = hosts[target].functions[1].clone();
                f.name.push_str("_insert");
                hosts[target].functions.push(f);
                "insert"
            }
            _ => unreachable!(),
        };
        steps.push(Step {
            hosts: hosts.clone(),
            event,
            target,
        });
    }
    steps
}
