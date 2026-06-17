use std::collections::HashSet;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use celeste::binel::{parser, writer, BinEl, BinElAttr, BinFile};
use clap::Parser;
use notify::{recommended_watcher, Event, RecursiveMode, Watcher};

#[derive(Parser, Debug, Clone)]
#[command(name = "celeste_bin_merge")]
#[command(about = "Merge two Celeste map .bin files by combining room definitions")]
struct Args {
    map_a: PathBuf,
    map_b: PathBuf,
    output: PathBuf,

    #[arg(long)]
    strict_meta: bool,

    #[arg(long)]
    rename_duplicates: bool,

    #[arg(long)]
    merge_filler: bool,

    #[arg(long)]
    check_overlaps: bool,

    #[arg(long)]
    watch: bool,
}

fn read_file(path: &PathBuf) -> Result<BinFile> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let (_rest, file) =
        parser::take_file(&bytes).map_err(|e| anyhow::anyhow!("failed to parse {}: {:?}", path.display(), e))?;
    Ok(file)
}

fn write_file(path: &PathBuf, file: &BinFile) -> Result<()> {
    let mut out = Cursor::new(Vec::<u8>::new());
    writer::put_file(&mut out, file).with_context(|| format!("failed to serialize {}", path.display()))?;
    fs::write(path, out.into_inner()).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn same_element(a: Option<&BinEl>, b: Option<&BinEl>) -> bool {
    a == b
}

fn room_name(room: &BinEl) -> Option<String> {
    match room.attributes.get("name") {
        Some(BinElAttr::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn room_pos(room: &BinEl) -> Option<(i32, i32)> {
    let x = match room.attributes.get("x") {
        Some(BinElAttr::Int(v)) => *v,
        _ => return None,
    };
    let y = match room.attributes.get("y") {
        Some(BinElAttr::Int(v)) => *v,
        _ => return None,
    };
    Some((x, y))
}

fn merge_levels(into_root: &mut BinEl, from_root: &BinEl, rename_duplicates: bool, check_overlaps: bool) -> Result<()> {
    let mut existing_names = HashSet::new();
    let mut existing_positions = HashSet::new();

    for room in into_root.get("levels").iter().flat_map(|levels| levels.children()) {
        if let Some(name) = room_name(room) {
            existing_names.insert(name);
        }
        if let Some(pos) = room_pos(room) {
            existing_positions.insert(pos);
        }
    }

    let source_levels = from_root.get("levels");
    if source_levels.is_empty() {
        return Ok(());
    }

    if into_root.get("levels").is_empty() {
        into_root.insert(BinEl::new("levels"));
    }

    let mut additions = Vec::new();

    for levels_node in source_levels {
        for room in levels_node.children() {
            let mut room = room.clone();

            if let Some(name) = room_name(&room) {
                if existing_names.contains(&name) {
                    if !rename_duplicates {
                        bail!("duplicate room name found: {}", name);
                    }

                    let mut i = 2usize;
                    let new_name = loop {
                        let candidate = format!("{}_merged{}", name, i);
                        if !existing_names.contains(&candidate) {
                            break candidate;
                        }
                        i += 1;
                    };

                    room.attributes
                        .insert("name".to_string(), BinElAttr::Text(new_name.clone()));
                    existing_names.insert(new_name);
                } else {
                    existing_names.insert(name);
                }
            }

            if check_overlaps {
                if let Some(pos) = room_pos(&room) {
                    if existing_positions.contains(&pos) {
                        eprintln!("warning: overlapping room position at x={}, y={}", pos.0, pos.1);
                    }
                    existing_positions.insert(pos);
                }
            }

            additions.push(room);
        }
    }

    let levels_vec = into_root.get_mut("levels");
    if levels_vec.is_empty() {
        levels_vec.push(BinEl::new("levels"));
    }

    for room in additions {
        levels_vec[0].insert(room);
    }

    Ok(())
}

fn merge_filler(into_root: &mut BinEl, from_root: &BinEl) {
    let source_filler = from_root.get("Filler");
    if source_filler.is_empty() {
        return;
    }

    if into_root.get("Filler").is_empty() {
        into_root.insert(BinEl::new("Filler"));
    }

    let mut additions = Vec::new();
    for filler_node in source_filler {
        for child in filler_node.children() {
            additions.push(child.clone());
        }
    }

    let filler_vec = into_root.get_mut("Filler");
    if filler_vec.is_empty() {
        filler_vec.push(BinEl::new("Filler"));
    }

    for child in additions {
        filler_vec[0].insert(child);
    }
}

fn first_named_child<'a>(root: &'a BinEl, name: &str) -> Option<&'a BinEl> {
    root.get(name).first()
}

fn build_once(args: &Args) -> Result<()> {
    let mut map_a = read_file(&args.map_a)?;
    let map_b = read_file(&args.map_b)?;

    if !same_element(first_named_child(&map_a.root, "Style"), first_named_child(&map_b.root, "Style")) {
        bail!("stylegrounds differ between the two maps");
    }

    let meta_equal = same_element(first_named_child(&map_a.root, "meta"), first_named_child(&map_b.root, "meta"));
    if !meta_equal && args.strict_meta {
        bail!("meta differs between the two maps");
    }
    if !meta_equal && !args.strict_meta {
        eprintln!("warning: meta differs between the two maps; keeping map_a meta");
    }

    if map_a.package != map_b.package {
        eprintln!(
            "warning: package names differ ({} vs {}); keeping {}",
            map_a.package, map_b.package, map_a.package
        );
    }

    merge_levels(&mut map_a.root, &map_b.root, args.rename_duplicates, args.check_overlaps)?;

    if args.merge_filler {
        merge_filler(&mut map_a.root, &map_b.root);
    }

    write_file(&args.output, &map_a)?;
    println!(
        "[{}] merged {} + {} -> {}",
        chrono_like_now(),
        args.map_a.display(),
        args.map_b.display(),
        args.output.display()
    );
    Ok(())
}

fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

fn path_matches(event: &Event, a: &Path, b: &Path) -> bool {
    event.paths.iter().any(|p| same_path(p, a) || same_path(p, b))
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(l), Ok(r)) => l == r,
        _ => left == right,
    }
}

fn watch_mode(args: Args) -> Result<()> {
    build_once(&args)?;

    let (tx, rx) = channel();

    let mut watcher = recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("failed to create file watcher")?;

    watcher
        .watch(&args.map_a, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", args.map_a.display()))?;
    watcher
        .watch(&args.map_b, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", args.map_b.display()))?;

    println!(
        "watching:\n  A: {}\n  B: {}\n  out: {}",
        args.map_a.display(),
        args.map_b.display(),
        args.output.display()
    );

    let mut last_build = Instant::now() - Duration::from_secs(10);
    let debounce = Duration::from_millis(250);

    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                if !path_matches(&event, &args.map_a, &args.map_b) {
                    continue;
                }

                let now = Instant::now();
                if now.duration_since(last_build) < debounce {
                    continue;
                }
                last_build = now;

                println!("change detected: {:?}", event.kind);
                if let Err(err) = build_once(&args) {
                    eprintln!("merge failed: {:#}", err);
                }
            }
            Ok(Err(err)) => {
                eprintln!("watch error: {:?}", err);
            }
            Err(err) => {
                bail!("watch channel closed: {}", err);
            }
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.watch {
        watch_mode(args)
    } else {
        build_once(&args)
    }
}