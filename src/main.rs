use clap::Parser;
use core::hash::BuildHasherDefault;
use fxhash::{FxHashMap, FxHasher};
use html_escape::encode_text;
use indexmap::IndexMap;
use md5::{Md5, Digest};
use base16ct;

use regex::Regex;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::fs::File;
use std::io::{self, BufRead};
use std::path::Path;
use std::path::PathBuf;
use tinytemplate::TinyTemplate;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use std::sync::Mutex;
use once_cell::sync::Lazy;

pub type FxIndexMap<K, V> = IndexMap<K, V, BuildHasherDefault<FxHasher>>;

static INTERN_TABLE: Lazy<Mutex<FxHashMap<u32, String>>> = Lazy::new(|| Mutex::new(FxHashMap::default()));

static CSS: &str = r#"
"#;

static TEMPLATE_INDEX: &str = r#"
<html>
<style>
{css}
</style>
<body>
<div>
<h2>Stack trie</h2>
{stack_trie_html | format_unescaped}
</div>
<div>
<h2>IR dumps</h2>
<ul>
{{ for compile_directory in directory }}
    <li>{compile_directory.0}
    <ul>
        {{ for path in compile_directory.1 }}
            <li><a href="{path}">{path}</a></li>
        {{ endfor }}
    </ul>
    </li>
{{ endfor }}
</ul>
</div>
</body>
</html>
"#;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    path: PathBuf,
    #[arg(short)]
    out: Option<PathBuf>,
    #[arg(long)]
    overwrite: bool,
}

#[derive(Default)]
struct StackTrieNode {
    terminal: Vec<String>,
    // Ordered map so that when we print we roughly print in chronological order
    children: FxIndexMap<FrameSummary, StackTrieNode>,
}

impl StackTrieNode {
    fn insert(&mut self, mut stack: StackSummary, compile_id: String) {
        let mut cur = self;
        for frame in stack.drain(..) {
            cur = cur.children.entry(frame).or_default();
        }
        cur.terminal.push(compile_id);
    }

    fn fmt_inner(&self, f: &mut Formatter, indent: usize) -> fmt::Result {
        for (frame, node) in self.children.iter() {
            let star = node.terminal.join("");
            if self.children.len() > 1 {
                // If the node has multiple children, increase the indent and print a hyphen
                writeln!(
                    f,
                    "{:indent$}- {star}{}",
                    "",
                    frame,
                    indent = indent,
                    star = star
                )?;
                node.fmt_inner(f, indent + 2)?;
            } else {
                // If the node has only one child, don't increase the indent and don't print a hyphen
                writeln!(
                    f,
                    "{:indent$}  {star}{}",
                    "",
                    frame,
                    indent = indent,
                    star = star
                )?;
                node.fmt_inner(f, indent)?;
            }
        }
        Ok(())
    }
}

impl Display for StackTrieNode {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "<pre>")?;
        self.fmt_inner(f, 0)?;
        write!(f, "</pre>")?;
        Ok(())
    }
}

#[derive(Eq, PartialEq, Hash, Deserialize, Serialize, Debug, Clone)]
struct CompileId {
    frame_id: u32,
    frame_compile_id: u32,
    attempt: u32,
}

impl fmt::Display for CompileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}/{}", self.frame_id, self.frame_compile_id)?;
        if self.attempt != 0 {
            write!(f, "_{}", self.attempt)?;
        }
        write!(f, "]")
    }
}

#[derive(Default, Debug)]
struct Stats {
    ok: u64,
    other_rank: u64,
    fail_glog: u64,
    fail_json: u64,
    fail_payload_md5: u64,
}

#[derive(Debug, Hash, Eq, PartialEq, Deserialize)]
struct FrameSummary {
    filename: u32,
    line: i32,
    name: String,
}

fn simplify_filename<'a>(filename: &'a str) -> &'a str {
    let parts: Vec<&'a str> = filename.split("#link-tree/").collect();
    if parts.len() > 1 {
        return parts[1];
    }
    // TODO: generalize this
    let parts: Vec<&'a str> = filename
        .split("1e322330-seed-nspid4026531836_cgpid26364902-ns-4026531840/")
        .collect();
    if parts.len() > 1 {
        return parts[1];
    }
    filename
}

impl fmt::Display for FrameSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let intern_table = INTERN_TABLE.lock().unwrap();
        let filename = intern_table.get(&self.filename).map_or("(unknown)", |s| s.as_str());
        write!(
            f,
            "{}:{} in {}",
            encode_text(simplify_filename(filename)),
            self.line,
            encode_text(&self.name)
        )
    }
}

type StackSummary = Vec<FrameSummary>;

#[derive(Debug, Deserialize)]
struct Envelope {
    rank: Option<u32>,
    #[serde(flatten)]
    compile_id: Option<CompileId>,
    #[serde(default)]
    has_payload: Option<String>,
    // externally tagged union, one field per log type we recognize
    compile_stack: Option<StackSummary>,
    dynamo_output_graph: Option<bool>,
    str: Option<(String, u32)>,
}

#[derive(Debug, Serialize)]
struct IndexContext {
    css: &'static str,
    directory: Vec<(String, Vec<PathBuf>)>,
    stack_trie_html: String,
}

fn main() {
    let cli = Cli::parse();
    let path = cli.path;
    let out_path = cli.out.unwrap_or(PathBuf::from("tl_out"));

    if out_path.exists() {
        if !cli.overwrite {
            panic!(
                "{} already exists, pass --overwrite to overwrite",
                out_path.display()
            );
        }
        fs::remove_dir_all(&out_path).unwrap();
    }
    fs::create_dir(&out_path).unwrap();

    let file = File::open(path).unwrap();
    let metadata = file.metadata().unwrap();
    let file_size = metadata.len();
    let multi = MultiProgress::new();
    let pb = multi.add(ProgressBar::new(file_size));
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} [{bytes_per_sec}] ({eta})").unwrap()
        .progress_chars("#>-"));
    let spinner = multi.add(ProgressBar::new_spinner());
    let reader = io::BufReader::new(file);

    let re_glog = Regex::new(concat!(
        r"(?<level>[VIWEC])(?<month>\d{2})(?<day>\d{2}) ",
        r"(?<hour>\d{2}):(?<minute>\d{2}):(?<second>\d{2}).(?<millisecond>\d{6}) ",
        r"(?<thread>\d+)",
        r"(?<pathname>[^:]+):(?<line>\d+)\] ",
        r"(?<payload>.)"
    ))
    .unwrap();

    let mut stack_trie = StackTrieNode::default();

    let mut stats = Stats::default();
    let _mod_count: FxHashMap<String, i32> = FxHashMap::default();

    let mut bytes_read: u64 = 0;

    // Some stuff for profiling
    let mut fastest_time = std::time::Duration::MAX;
    let mut slowest_time = std::time::Duration::ZERO;

    let mut expected_rank: Option<Option<u32>> = None;

    let mut directory: FxHashMap<Option<CompileId>, Vec<PathBuf>> = FxHashMap::default();
    let mut intern_table: FxHashMap<u32, String> = FxHashMap::default();

    let mut iter = reader.lines().peekable();

    while let Some(line) = iter.next() {
        let line = line.unwrap();
        bytes_read += line.len() as u64;
        pb.set_position(bytes_read);
        spinner.set_message(format!("{:?}", stats));
        //spinner.set_message(format!("{:?} {:?}", slowest_time, fastest_time));
        let start = Instant::now();

        let Some(caps) = re_glog.captures(&line) else {
            stats.fail_glog += 1;
            continue;
        };

        let end = start.elapsed();
        if end < fastest_time {
            fastest_time = end;
        }
        if end > slowest_time {
            slowest_time = end;
            //println!("{}", line);
        }
        let payload = &line[caps.name("payload").unwrap().start()..];

        let e = match serde_json::from_str::<Envelope>(payload) {
            Ok(r) => r,
            Err(err) => {
                multi.suspend(|| {
                    eprintln!("{}\n{:?}", payload, err);
                });
                stats.fail_json += 1;
                continue;
            }
        };

        match expected_rank {
            Some(rank) => {
                if rank != e.rank {
                    stats.other_rank += 1;
                    continue;
                }
            }
            None => {
                multi.suspend(|| {
                    eprintln!("Detected rank: {:?}", e.rank);
                });
                expected_rank = Some(e.rank);
            }
        };

        // TODO: borrow only here
        let compile_id_dir = e
            .compile_id
            .clone()
            .map_or("unknown".to_string(), |e: CompileId| {
                format!("{}_{}_{}", e.frame_id, e.frame_compile_id, e.attempt)
            });

        let subdir = out_path.join(&compile_id_dir);
        fs::create_dir_all(&subdir).unwrap();
        let compile_directory = directory.entry(e.compile_id).or_default();

        let mut payload = String::new();
        if let Some(expect) = e.has_payload {
            let mut first = true;
            while let Some(Ok(payload_line)) = iter.next_if(|l| l.as_ref().map_or_else(|_| false, |l| l.starts_with('\t'))) {
                // Careful! Distinguish between missing EOL and not
                if !first {
                    payload.push_str("\n");
                }
                first = false;
                payload.push_str(&payload_line[1..]);
            }
            let mut hasher = Md5::new();
            hasher.update(&payload);
            let hash = hasher.finalize();
            let mut expect_buf = [0u8; 16];
            if let Ok(_) = base16ct::lower::decode(expect, &mut expect_buf) {
                if expect_buf != &hash[..] {
                    // TODO: error log
                    stats.fail_payload_md5 += 1;
                }
            } else {
                stats.fail_payload_md5 += 1;
            }
        }

        stats.ok += 1;
        if let Some(stack) = e.compile_stack {
            stack_trie.insert(stack, "* ".to_string()); // TODO: compile id
        };

        if let Some((s, i)) = e.str {
            let mut intern_table = INTERN_TABLE.lock().unwrap();
            intern_table.insert(i, s);
        };

        if let Some(_r) = e.dynamo_output_graph {
            let filename = "dynamo_output_graph.txt";
            let f = subdir.join(filename);
            fs::write(&f, payload).unwrap();
            compile_directory.push(Path::new(&compile_id_dir).join(filename));
        };
    }
    pb.finish_with_message("done");
    spinner.finish();

    eprintln!("{:?}", stats);

    let mut tt = TinyTemplate::new();
    tt.add_formatter("format_unescaped", tinytemplate::format_unescaped);
    tt.add_template("index.html", TEMPLATE_INDEX).unwrap();
    let index_context = IndexContext {
        css: CSS,
        directory: directory
            .drain()
            .map(|(x, y)| (x.map_or("(unknown)".to_string(), |e| e.to_string()), y))
            .collect(),
        stack_trie_html: stack_trie.to_string(),
    };
    fs::write(
        out_path.join("index.html"),
        tt.render("index.html", &index_context).unwrap(),
    )
    .unwrap();

    opener::open(out_path.join("index.html")).unwrap();
}
