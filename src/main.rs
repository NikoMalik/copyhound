use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::process::ExitCode;

const USAGE: &str = "\
usage: copyhound memcpy|memmove|memset [--bytes N] [--context K] [--filter TEXT] [file.ll]
       copyhound funcsize [--filter TEXT] [file.ll]
       copyhound panics [--deny] [--filter TEXT] [file.ll]
       copyhound calls [--filter TEXT] [file.ll]
       copyhound indirect [--filter TEXT] [file.ll]
       copyhound diff [--bytes N] [--filter TEXT] before.ll after.ll

memcpy    every memcpy with a known size above N bytes (default 128): function size
memmove   same for memmove
memset    same for memset
funcsize  every function with its IR line count: function lines
panics    every function that calls a panic: function bounds=N unwrap=N overflow=N other=N
calls     every function with the functions it calls directly and how many times
indirect  every function that calls through a pointer: function count
diff      functions that changed between two IR files, generic instances are summed by name

--bytes N      copy size threshold for memcpy, memmove, memset and diff
--context K    also print the K IR lines before each copy
--filter TEXT  only functions whose demangled name contains TEXT
--deny         exit with code 1 when panics finds any panic

reads LLVM IR from the file or stdin, exit code 2 on errors
rust: cargo rustc --release -- --emit=llvm-ir (the .ll lands next to the binary in deps/)
zig: zig build-exe -O ReleaseFast -femit-llvm-ir=app.ll main.zig, or install exe.getEmittedLlvmIr() in build.zig
rust names are demangled, zig comptime arguments are stripped
";

const DEFAULT_BYTES: u64 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyKind {
    Memcpy,
    Memmove,
    Memset,
}

impl CopyKind {
    const ALL: [CopyKind; 3] = [CopyKind::Memcpy, CopyKind::Memmove, CopyKind::Memset];

    fn intrinsic(self) -> &'static str {
        match self {
            CopyKind::Memcpy => "@llvm.memcpy",
            CopyKind::Memmove => "@llvm.memmove",
            CopyKind::Memset => "@llvm.memset",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Copy(CopyKind),
    FuncSize,
    Panics,
    Calls,
    Indirect,
    Diff,
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    mode: Mode,
    bytes: u64,
    context: usize,
    filter: Option<String>,
    deny: bool,
    paths: Vec<String>,
}

enum Parsed {
    Run(Options),
    Help,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let mut mode = None;
    let mut bytes = None;
    let mut context = 0;
    let mut filter = None;
    let mut deny = false;
    let mut paths = Vec::new();
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "memcpy" => mode = Some(Mode::Copy(CopyKind::Memcpy)),
            "memmove" => mode = Some(Mode::Copy(CopyKind::Memmove)),
            "memset" => mode = Some(Mode::Copy(CopyKind::Memset)),
            "funcsize" => mode = Some(Mode::FuncSize),
            "panics" => mode = Some(Mode::Panics),
            "calls" => mode = Some(Mode::Calls),
            "indirect" => mode = Some(Mode::Indirect),
            "diff" => mode = Some(Mode::Diff),
            "--deny" => deny = true,
            "--bytes" | "--context" | "--filter" => {
                let value = args.next().ok_or_else(|| format!("{a} needs a value"))?;
                match a.as_str() {
                    "--bytes" => {
                        bytes = Some(
                            value
                                .parse()
                                .map_err(|_| format!("bad --bytes '{value}'"))?,
                        );
                    }
                    "--context" => {
                        context = value
                            .parse()
                            .map_err(|_| format!("bad --context '{value}'"))?;
                    }
                    _ => filter = Some(value),
                }
            }
            "-h" | "--help" => return Ok(Parsed::Help),
            _ if a.starts_with("--") => return Err(format!("unknown flag '{a}'")),
            _ => paths.push(a),
        }
    }
    let mode = mode.ok_or_else(|| format!("missing mode\n{USAGE}"))?;
    let is_copy = matches!(mode, Mode::Copy(_));
    if bytes.is_some() && !is_copy && mode != Mode::Diff {
        return Err("--bytes only works with memcpy, memmove, memset and diff".to_owned());
    }
    if context != 0 && !is_copy {
        return Err("--context only works with memcpy, memmove and memset".to_owned());
    }
    if deny && mode != Mode::Panics {
        return Err("--deny only works with panics".to_owned());
    }
    match (mode, paths.len()) {
        (Mode::Diff, 2) => {}
        (Mode::Diff, _) => return Err("diff needs two files: before.ll after.ll".to_owned()),
        (_, 0 | 1) => {}
        _ => return Err("only one input file is allowed".to_owned()),
    }
    Ok(Parsed::Run(Options {
        mode,
        bytes: bytes.unwrap_or(DEFAULT_BYTES),
        context,
        filter,
        deny,
        paths,
    }))
}

fn main() -> ExitCode {
    let opts = match parse_args(std::env::args().skip(1)) {
        Ok(Parsed::Run(opts)) => opts,
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::from(2);
        }
    };
    let mut out = BufWriter::with_capacity(64 * 1024, io::stdout().lock());
    match run(&opts, &mut out).and_then(|found| out.flush().map(|()| found)) {
        Ok(found) if opts.deny && found > 0 => {
            eprintln!("copyhound: {found} functions call a panic");
            ExitCode::from(1)
        }
        Ok(_) => ExitCode::SUCCESS,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

fn open(path: Option<&str>) -> io::Result<Box<dyn BufRead>> {
    Ok(match path {
        Some(p) => {
            let f = File::open(p)
                .map_err(|e| io::Error::new(e.kind(), format!("cannot open {p}: {e}")))?;
            Box::new(BufReader::with_capacity(1 << 20, f))
        }
        None => Box::new(BufReader::with_capacity(1 << 20, io::stdin().lock())),
    })
}

fn run(opts: &Options, out: &mut impl Write) -> io::Result<usize> {
    if opts.mode == Mode::Diff {
        let before = collect(open(Some(&opts.paths[0]))?, opts)?;
        let after = collect(open(Some(&opts.paths[1]))?, opts)?;
        report_diff(out, &before, &after, opts.bytes)?;
        return Ok(0);
    }
    let funcs = collect(open(opts.paths.first().map(String::as_str))?, opts)?;
    report(out, &funcs, opts.mode)
}

#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
struct PanicCounts {
    bounds: u32,
    unwrap: u32,
    overflow: u32,
    other: u32,
}

impl PanicCounts {
    fn total(&self) -> u32 {
        self.bounds + self.unwrap + self.overflow + self.other
    }

    fn add(&mut self, kind: PanicKind) {
        match kind {
            PanicKind::Bounds => self.bounds += 1,
            PanicKind::Unwrap => self.unwrap += 1,
            PanicKind::Overflow => self.overflow += 1,
            PanicKind::Other => self.other += 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanicKind {
    Bounds,
    Unwrap,
    Overflow,
    Other,
}

#[derive(Debug)]
struct CopyHit {
    kind: CopyKind,
    bytes: u64,
    context: Vec<String>,
}

#[derive(Debug, Default)]
struct FuncStats {
    name: String,
    lines: u64,
    copies: Vec<CopyHit>,
    panics: PanicCounts,
    indirect: u32,
    calls: BTreeMap<String, u32>,
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn collect(mut input: impl BufRead, opts: &Options) -> io::Result<Vec<FuncStats>> {
    let mut funcs = Vec::new();
    let mut raw = Vec::new();
    let mut current: Option<(FuncStats, bool)> = None;
    let mut recent: VecDeque<String> = VecDeque::with_capacity(opts.context);
    loop {
        raw.clear();
        if input.read_until(b'\n', &mut raw)? == 0 {
            return Ok(funcs);
        }
        let text = String::from_utf8_lossy(&raw);
        let line = text.trim_end_matches(['\n', '\r']);

        if line.starts_with("define ") {
            let name = function_name(line)
                .ok_or_else(|| invalid(format!("cannot parse define line: {line}")))?;
            let name = demangle(&name);
            let shown = opts.filter.as_deref().is_none_or(|f| name.contains(f));
            current = Some((
                FuncStats {
                    name,
                    ..FuncStats::default()
                },
                shown,
            ));
            recent.clear();
            continue;
        }
        let Some((func, shown)) = current.as_mut() else {
            continue;
        };
        if line == "}" {
            if let Some((func, true)) = current.take() {
                funcs.push(func);
            }
            continue;
        }
        func.lines += 1;
        if !*shown {
            continue;
        }
        for kind in CopyKind::ALL {
            let Some((_, call)) = line.split_once(kind.intrinsic()) else {
                continue;
            };
            let bytes = copy_size(call)
                .ok_or_else(|| invalid(format!("cannot parse {kind:?} call: {line}")))?;
            if bytes > opts.bytes {
                let mut context: Vec<String> = recent.iter().cloned().collect();
                if opts.context > 0 {
                    context.push(line.trim().to_owned());
                }
                func.copies.push(CopyHit {
                    kind,
                    bytes,
                    context,
                });
            }
        }
        match call_target(line) {
            Some(Target::Direct(sym)) if !sym.starts_with("llvm.") => {
                let callee = demangle(sym);
                if let Some(kind) = panic_kind(&callee) {
                    func.panics.add(kind);
                }
                *func.calls.entry(callee).or_default() += 1;
            }
            Some(Target::Indirect) => func.indirect += 1,
            _ => {}
        }
        if opts.context > 0 && !line.trim().is_empty() {
            if recent.len() == opts.context {
                recent.pop_front();
            }
            recent.push_back(line.trim().to_owned());
        }
    }
}

fn report(out: &mut impl Write, funcs: &[FuncStats], mode: Mode) -> io::Result<usize> {
    let mut found = 0;
    for f in funcs {
        match mode {
            Mode::Copy(kind) => {
                for hit in f.copies.iter().filter(|h| h.kind == kind) {
                    writeln!(out, "{} {}", f.name, hit.bytes)?;
                    for l in &hit.context {
                        writeln!(out, "    {l}")?;
                    }
                }
            }
            Mode::FuncSize => writeln!(out, "{} {}", f.name, f.lines)?,
            Mode::Panics => {
                let p = &f.panics;
                if p.total() > 0 {
                    found += 1;
                    writeln!(
                        out,
                        "{} bounds={} unwrap={} overflow={} other={}",
                        f.name, p.bounds, p.unwrap, p.overflow, p.other
                    )?;
                }
            }
            Mode::Calls => {
                if f.calls.is_empty() && f.indirect == 0 {
                    continue;
                }
                writeln!(out, "{}", f.name)?;
                let mut calls: Vec<(&String, &u32)> = f.calls.iter().collect();
                calls.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
                for (callee, n) in calls {
                    writeln!(out, "    {n} {callee}")?;
                }
                if f.indirect > 0 {
                    writeln!(out, "    {} <indirect>", f.indirect)?;
                }
            }
            Mode::Indirect => {
                if f.indirect > 0 {
                    writeln!(out, "{} {}", f.name, f.indirect)?;
                }
            }
            Mode::Diff => unreachable!("diff is reported by report_diff"),
        }
    }
    Ok(found)
}

#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
struct Summary {
    lines: u64,
    copies: u64,
    panics: u32,
    indirect: u32,
}

fn summarize(funcs: &[FuncStats], bytes: u64) -> BTreeMap<&str, Summary> {
    let mut map: BTreeMap<&str, Summary> = BTreeMap::new();
    for f in funcs {
        let s = map.entry(f.name.as_str()).or_default();
        s.lines += f.lines;
        s.copies += f.copies.iter().filter(|h| h.bytes > bytes).count() as u64;
        s.panics += f.panics.total();
        s.indirect += f.indirect;
    }
    map
}

fn report_diff(
    out: &mut impl Write,
    before: &[FuncStats],
    after: &[FuncStats],
    bytes: u64,
) -> io::Result<()> {
    let old = summarize(before, bytes);
    let new = summarize(after, bytes);
    let names: BTreeSet<&str> = old.keys().chain(new.keys()).copied().collect();
    for name in names {
        match (old.get(name), new.get(name)) {
            (Some(a), Some(b)) if a != b => writeln!(
                out,
                "changed {name} lines {} -> {} copies {} -> {} panics {} -> {} indirect {} -> {}",
                a.lines, b.lines, a.copies, b.copies, a.panics, b.panics, a.indirect, b.indirect
            )?,
            (None, Some(b)) => writeln!(
                out,
                "added {name} lines {} copies {} panics {} indirect {}",
                b.lines, b.copies, b.panics, b.indirect
            )?,
            (Some(a), None) => writeln!(
                out,
                "removed {name} lines {} copies {} panics {} indirect {}",
                a.lines, a.copies, a.panics, a.indirect
            )?,
            _ => {}
        }
    }
    Ok(())
}

fn demangle(name: &str) -> String {
    format!("{:#}", rustc_demangle::demangle(name))
}

fn function_name(define: &str) -> Option<String> {
    if !define.ends_with('{') {
        return None;
    }
    let (_, mangled) = define.split_once('@')?;
    let mut name = String::new();
    let mut level = 0u32;
    for c in mangled.chars() {
        match c {
            '(' => level += 1,
            ')' => level = level.saturating_sub(1),
            '"' => {}
            _ if level > 0 => {}
            ' ' => return Some(name),
            _ => name.push(c),
        }
    }
    None
}

fn copy_size(call: &str) -> Option<u64> {
    let (_, args) = call.split_once('(')?;
    let mut level = 0u32;
    let mut arg = 0u32;
    let mut start = 0;
    for (i, c) in args.char_indices() {
        match c {
            '(' => level += 1,
            ')' if level == 0 => {
                return if arg == 2 {
                    size_value(&args[start..i])
                } else {
                    None
                };
            }
            ')' => level -= 1,
            ',' if level == 0 => {
                if arg == 2 {
                    return size_value(&args[start..i]);
                }
                arg += 1;
                start = i + 1;
            }
            _ => {}
        }
    }
    None
}

fn size_value(arg: &str) -> Option<u64> {
    let value = arg.trim().rsplit(' ').next()?;
    if value.starts_with('%') {
        return Some(0);
    }
    value.parse().ok()
}

#[derive(Debug, PartialEq, Eq)]
enum Target<'a> {
    Direct(&'a str),
    Indirect,
}

fn call_target(line: &str) -> Option<Target<'_>> {
    let mut t = line.trim_start();
    if t.starts_with('%') {
        t = t.split_once(" = ")?.1;
    }
    let rest = [
        "tail call ",
        "musttail call ",
        "notail call ",
        "call ",
        "invoke ",
    ]
    .iter()
    .find_map(|p| t.strip_prefix(p))?;
    let b = rest.as_bytes();
    let mut level = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'(' | b'{' | b'[' | b'<' => level += 1,
            b')' | b'}' | b']' | b'>' => level -= 1,
            b'"' => i += rest[i + 1..].find('"')? + 1,
            c @ (b'@' | b'%') if level == 0 && (i == 0 || b[i - 1] == b' ') => {
                let start = i + 1;
                let (name, end) = if b.get(start) == Some(&b'"') {
                    let close = start + 1 + rest[start + 1..].find('"')?;
                    (&rest[start + 1..close], close + 1)
                } else {
                    let len = rest[start..]
                        .find(['(', ' ', ',', ')'])
                        .unwrap_or(rest.len() - start);
                    (&rest[start..start + len], start + len)
                };
                if b.get(end) == Some(&b'(') {
                    return Some(if c == b'@' {
                        Target::Direct(name)
                    } else {
                        Target::Indirect
                    });
                }
                i = end;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn panic_kind(callee: &str) -> Option<PanicKind> {
    if callee.contains("panic_count") || callee.contains("panic_in_cleanup") {
        return None;
    }
    if callee.contains("panic_bounds_check")
        || callee.starts_with("core::slice::index::")
        || callee.contains("slice_error_fail")
    {
        return Some(PanicKind::Bounds);
    }
    if callee.contains("unwrap_failed") || callee.contains("expect_failed") {
        return Some(PanicKind::Unwrap);
    }
    if callee.contains("panic_const") {
        return Some(PanicKind::Overflow);
    }
    if callee.contains("panic") || callee.contains("capacity_overflow") {
        return Some(PanicKind::Other);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Options {
        match parse_args(args.iter().map(|s| (*s).to_owned())) {
            Ok(Parsed::Run(o)) => o,
            _ => panic!("bad test args {args:?}"),
        }
    }

    fn run_on(ir: &str, args: &[&str]) -> (String, usize) {
        let o = opts(args);
        let funcs = collect(ir.as_bytes(), &o).expect("collect");
        let mut out = Vec::new();
        let found = report(&mut out, &funcs, o.mode).expect("report");
        (String::from_utf8(out).expect("utf8"), found)
    }

    fn run(ir: &str, args: &[&str]) -> String {
        run_on(ir, args).0
    }

    #[test]
    fn function_name_strips_comptime_arguments_and_quotes() {
        assert_eq!(
            function_name(
                "define internal fastcc i64 @\".vsr.vsr.clock.ClockType(.vsr.time.Time).monotonic\"\
                 (%.vsr.time.Time* %.0.1.val) unnamed_addr #1 !dbg !71485 {"
            )
            .as_deref(),
            Some(".vsr.vsr.clock.ClockType.monotonic")
        );
        assert_eq!(
            function_name(
                "define internal fastcc i16 @proto.handler.ShardHandler.serveGet(ptr %0, ptr %1) unnamed_addr #0 {"
            )
            .as_deref(),
            Some("proto.handler.ShardHandler.serveGet")
        );
        assert_eq!(function_name("define void @f()"), None);
    }

    #[test]
    fn rust_names_are_demangled() {
        let line = "define internal fastcc void @_ZN8probemap27SwissTable$LT$E$C$S$C$A$GT$12insert_inner17hc080e40e5eb47e28E(ptr %self) unnamed_addr #3 {";
        let name = function_name(line).expect("name");
        assert_eq!(demangle(&name), "probemap::SwissTable<E,S,A>::insert_inner");
    }

    #[test]
    fn copy_size_reads_the_known_size() {
        assert_eq!(
            copy_size(
                ".p0i8.p0i8.i64(i8* align 8 %0, i8* align 8 bitcast({ void (i32, %std.os.linux.siginfo_t*, i8*)*, \
                 [32 x i32], <{ i32, [4 x i8] }>, void ()* }* @8 to i8*), i64 152, i1 false)"
            ),
            Some(152)
        );
        assert_eq!(
            copy_size(".p0i8.p0i8.i64(i8* align 1 %8, i8* align 1 %4, i64 %6, i1 false)"),
            Some(0)
        );
        assert_eq!(
            copy_size(".p0.p0.i64(ptr align 8 %5, ptr align 8 %6, i64 4096, i1 false), !dbg !12"),
            Some(4096)
        );
        assert_eq!(
            copy_size(
                ".p0.p0.i64(ptr align 1 %625, ptr nonnull readonly align 1 %4, i64 range(i64 0, 4294967296) %5, i1 false)"
            ),
            Some(0)
        );
        assert_eq!(
            copy_size(
                ".p0.p0.i64(ptr noundef nonnull align 8 dereferenceable(200) %a, ptr align 8 %b, i64 200, i1 false)"
            ),
            Some(200)
        );
        assert_eq!(
            copy_size(".p0.i64(ptr align 16 %buf, i8 0, i64 8192, i1 false)"),
            Some(8192)
        );
    }

    #[test]
    fn call_target_finds_the_called_function() {
        let d = |l| call_target(l);
        assert_eq!(d("  call void @f(ptr %x)"), Some(Target::Direct("f")));
        assert_eq!(
            d("  %r = tail call fastcc noundef i64 @\"_ZN3foo3barE\"(i64 1)"),
            Some(Target::Direct("_ZN3foo3barE"))
        );
        assert_eq!(
            d("  %5 = call { i64, ptr } @g(ptr %0)"),
            Some(Target::Direct("g"))
        );
        assert_eq!(
            d("  %v = call i32 (ptr, ...) @printf(ptr @fmt, i32 %x)"),
            Some(Target::Direct("printf"))
        );
        assert_eq!(
            d("  %s = call %\"core::fmt::Arguments\" @args(ptr %0)"),
            Some(Target::Direct("args"))
        );
        assert_eq!(
            d("  call void %fnptr(ptr @_ZN4core9panicking9panic_fmt17hE)"),
            Some(Target::Indirect)
        );
        assert_eq!(d("  %r = call i64 %8(ptr %self)"), Some(Target::Indirect));
        assert_eq!(
            d("  call void asm sideeffect \"\", \"r,~{memory}\"(ptr %x)"),
            None
        );
        assert_eq!(
            d("  invoke void @h(ptr %0)\n          to label %ok unwind label %bad"),
            Some(Target::Direct("h"))
        );
        assert_eq!(d("  store i64 %x, ptr @g, align 8"), None);
    }

    const IR: &str = "\
define internal void @a.big(ptr %0) unnamed_addr #0 {
  %x = load i64, ptr %0, align 8
  call void @llvm.memcpy.p0.p0.i64(ptr align 8 %0, ptr align 8 %1, i64 4096, i1 false)
  call void @llvm.memcpy.p0.p0.i64(ptr align 8 %0, ptr align 8 %1, i64 16, i1 false)
  ret void
}
define internal void @\"b.Gen(u8).small\"() unnamed_addr #0 {
  ret void
}
";

    #[test]
    fn memcpy_and_funcsize_match_the_zig_tool() {
        assert_eq!(run(IR, &["memcpy"]), "a.big 4096\n");
        assert_eq!(run(IR, &["funcsize"]), "a.big 4\nb.Gen.small 1\n");
        assert_eq!(
            run(IR, &["memcpy", "--bytes", "8"]),
            "a.big 4096\na.big 16\n"
        );
    }

    #[test]
    fn memcpy_context_shows_previous_lines() {
        assert_eq!(
            run(IR, &["memcpy", "--context", "1"]),
            "a.big 4096\n    %x = load i64, ptr %0, align 8\n    \
             call void @llvm.memcpy.p0.p0.i64(ptr align 8 %0, ptr align 8 %1, i64 4096, i1 false)\n"
        );
    }

    #[test]
    fn memmove_and_memset_have_own_modes() {
        let ir = "\
define void @z() {
  call void @llvm.memset.p0.i64(ptr align 16 %buf, i8 0, i64 8192, i1 false)
  call void @llvm.memmove.p0.p0.i64(ptr align 8 %a, ptr align 8 %b, i64 512, i1 false)
  call void @llvm.memcpy.p0.p0.i64(ptr align 8 %a, ptr align 8 %b, i64 256, i1 false)
  ret void
}
";
        assert_eq!(run(ir, &["memset"]), "z 8192\n");
        assert_eq!(run(ir, &["memmove"]), "z 512\n");
        assert_eq!(run(ir, &["memcpy"]), "z 256\n");
        assert_eq!(run(ir, &["memmove", "--bytes", "1000"]), "");
    }

    #[test]
    fn filter_keeps_matching_functions() {
        assert_eq!(
            run(IR, &["funcsize", "--filter", "small"]),
            "b.Gen.small 1\n"
        );
    }

    const CALLS_IR: &str = "\
define void @_ZN3foo3get17h0123456789abcdefE(ptr %0) unnamed_addr #0 {
  tail call void @_ZN4core9panicking18panic_bounds_check17h0123456789abcdefE(i64 %i, i64 4, ptr @loc) #9
  %r = invoke i64 @_ZN4core6option13expect_failed17h0123456789abcdefE(ptr @msg, i64 3, ptr @loc)
          to label %ok unwind label %bad
  call void @_ZN4core9panicking11panic_const24panic_const_add_overflow17h0123456789abcdefE(ptr @loc)
  call void @_ZN3std9panicking11panic_count17is_zero_slow_path17h0123456789abcdefE()
  call void @\"_ZN4core5slice5index24slice_end_index_len_fail17h0123456789abcdefE\"(i64 1, i64 2, ptr @loc)
  call void @_ZN4core9panicking9panic_fmt17h0123456789abcdefE(ptr %a, ptr @loc)
  call void %fnptr(ptr @_ZN4core9panicking9panic_fmt17h0123456789abcdefE)
  call void @helper(ptr %0)
  call void @helper(ptr %1)
  ret void
}
define void @clean() {
  call void @llvm.memcpy.p0.p0.i64(ptr align 8 %0, ptr align 8 %1, i64 16, i1 false)
  %r = call i64 %vtable_fn(ptr %0)
  ret void
}
";

    #[test]
    fn panics_are_counted_per_function() {
        assert_eq!(
            run(CALLS_IR, &["panics"]),
            "foo::get bounds=2 unwrap=1 overflow=1 other=1\n"
        );
        assert_eq!(run_on(CALLS_IR, &["panics", "--deny"]).1, 1);
        assert_eq!(
            run_on(CALLS_IR, &["panics", "--deny", "--filter", "clean"]).1,
            0
        );
    }

    #[test]
    fn calls_and_indirect_list_callees() {
        assert_eq!(
            run(CALLS_IR, &["calls", "--filter", "get"]),
            "foo::get\n    2 helper\n    1 core::option::expect_failed\n    1 core::panicking::panic_bounds_check\n    \
             1 core::panicking::panic_const::panic_const_add_overflow\n    1 core::panicking::panic_fmt\n    \
             1 core::slice::index::slice_end_index_len_fail\n    1 std::panicking::panic_count::is_zero_slow_path\n    \
             1 <indirect>\n"
        );
        assert_eq!(
            run(CALLS_IR, &["calls", "--filter", "clean"]),
            "clean\n    1 <indirect>\n"
        );
        assert_eq!(run(CALLS_IR, &["indirect"]), "foo::get 1\nclean 1\n");
    }

    #[test]
    fn diff_shows_changed_added_and_removed() {
        let before = "\
define void @keep() {
  ret void
}
define void @grow() {
  call void @llvm.memcpy.p0.p0.i64(ptr align 8 %0, ptr align 8 %1, i64 4096, i1 false)
  ret void
}
define void @gone() {
  ret void
}
";
        let after = "\
define void @keep() {
  ret void
}
define void @grow() {
  call void @_ZN4core9panicking18panic_bounds_check17h0123456789abcdefE(i64 %i, i64 4, ptr @loc)
  call void @llvm.memcpy.p0.p0.i64(ptr align 8 %0, ptr align 8 %1, i64 4096, i1 false)
  call void @llvm.memcpy.p0.p0.i64(ptr align 8 %0, ptr align 8 %1, i64 4096, i1 false)
  ret void
}
define void @new() {
  %r = call i64 %f(ptr %0)
  ret void
}
";
        let o = opts(&["diff", "a.ll", "b.ll"]);
        let a = collect(before.as_bytes(), &o).expect("before");
        let b = collect(after.as_bytes(), &o).expect("after");
        let mut out = Vec::new();
        report_diff(&mut out, &a, &b, o.bytes).expect("diff");
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "removed gone lines 1 copies 0 panics 0 indirect 0\n\
             changed grow lines 2 -> 4 copies 1 -> 2 panics 0 -> 1 indirect 0 -> 0\n\
             added new lines 2 copies 0 panics 0 indirect 1\n"
        );
    }

    #[test]
    fn diff_sums_generic_instances_with_the_same_name() {
        let ir = "\
define void @_ZN3foo3get17h1111111111111111E() {
  ret void
}
define void @_ZN3foo3get17h2222222222222222E() {
  ret void
}
";
        let o = opts(&["diff", "a.ll", "b.ll"]);
        let funcs = collect(ir.as_bytes(), &o).expect("collect");
        assert_eq!(summarize(&funcs, o.bytes)["foo::get"].lines, 2);
    }

    #[test]
    fn bad_args_are_rejected() {
        let parse = |a: &[&str]| parse_args(a.iter().map(|s| (*s).to_owned())).err();
        assert!(parse(&[]).is_some());
        assert!(parse(&["memcpy", "--bytes"]).is_some());
        assert!(parse(&["memcpy", "--bytes", "x"]).is_some());
        assert!(parse(&["panics", "--context", "2"]).is_some());
        assert!(parse(&["memcpy", "--deny"]).is_some());
        assert!(parse(&["funcsize", "--bytes", "4"]).is_some());
        assert!(parse(&["diff", "a.ll"]).is_some());
        assert!(parse(&["memcpy", "a.ll", "b.ll"]).is_some());
        assert!(parse(&["memcpy", "--nope"]).is_some());
    }
}
