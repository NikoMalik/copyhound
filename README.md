# copyhound

Finds big copies, panics and bounds checks in LLVM IR (`.ll`). Tested with Rust and Zig.

## Install

```sh
cargo install --path .
```

## Get the IR

Rust:

```sh
cargo rustc --release -- --emit=llvm-ir
# the .ll lands next to the binary: target/release/deps/<crate>-<hash>.ll
# or under $CARGO_TARGET_DIR/release/deps if it is set
```

Generic code only shows up where it is used with concrete types, so for a library
emit IR of a bench, an example or a small binary that calls it:

```sh
cargo rustc --release --bench bench -- --emit=llvm-ir
```

Zig, a single file:

```sh
zig build-exe -O ReleaseFast -femit-llvm-ir=app.ll src/main.zig
```

Zig with `build.zig`, install the IR next to the binary:

```zig
const exe = b.addExecutable(.{ ... });
b.getInstallStep().dependOn(&b.addInstallBinFile(exe.getEmittedLlvmIr(), "app.ll").step);
```

```sh
zig build -Doptimize=ReleaseFast   # zig-out/bin/app.ll
```

## Modes

All example outputs below are real runs on the IR of the benchmark of
[probemap](https://github.com/NikoMalik/probemap), a SwissTable hash map, built with
`cargo rustc --release --bench bench -- --emit=llvm-ir` (`bench.ll` in the examples).
The `criterion::` functions come from the criterion crate the benchmark uses.

```sh
copyhound memcpy|memmove|memset [--bytes N] [--context K] [--filter TEXT] [file.ll]
copyhound funcsize [--filter TEXT] [file.ll]
copyhound panics [--deny] [--filter TEXT] [file.ll]
copyhound calls [--filter TEXT] [file.ll]
copyhound indirect [--filter TEXT] [file.ll]
copyhound diff [--bytes N] [--filter TEXT] before.ll after.ll
```

Without `file.ll` it reads stdin. Exit code is 2 on errors, 1 when `panics --deny` finds a panic.

`memcpy` prints every `memcpy` with a size known at compile time above `N` bytes (default 128):

```
$ copyhound memcpy --bytes 16 --filter probemap bench.ll
probemap::SwissTable<E,S,A>::insert_inner 32
probemap::SwissTable<E,S,A>::insert_inner 32
probemap::SwissTable<E,S,A>::reserve_rehash 32
probemap::SwissTable<E,S,A>::rehash_in_place 32
probemap::SwissTable<E,S,A>::with_capacity_in 40
probemap::SwissTable<E,S,A>::with_capacity_in 40
```

The same function can show up more than once, one line per generic instance or per copy inside it.
`memmove` and `memset` work the same way for those intrinsics.

`--context K` adds the `K` IR lines before each copy, so you can see where it comes from:

```
$ copyhound memcpy --bytes 16 --context 2 --filter insert_inner bench.ll
probemap::SwissTable<E,S,A>::insert_inner 32
    %_103 = shl i64 %idx.sroa.0.043, 5
    %_100 = getelementptr inbounds nuw i8, ptr %_77, i64 %_103
    tail call void @llvm.memcpy.p0.p0.i64(ptr noundef nonnull align 8 dereferenceable(32) %_100, ptr noundef nonnull align 8 dereferenceable(32) %value, i64 32, i1 false)
```

`funcsize` prints every function with its IR line count, pipe it to `sort -k2 -n -r` to see the biggest:

```
$ copyhound funcsize --filter probemap bench.ll | sort -k2 -n -r | head -3
probemap::SwissTable<E,S,A>::rehash_in_place 422
probemap::SwissTable<E,S,A>::reserve_rehash 330
probemap::SwissTable<E,S,A>::rehash_in_place 295
```

`panics` prints every function that calls a panic, inlined code included:

```
$ copyhound panics --filter criterion bench.ll
criterion::Criterion<M>::configure_from_args bounds=1 unwrap=1 overflow=0 other=2
criterion::fs::save_string bounds=1 unwrap=0 overflow=0 other=0
criterion::stats::univariate::resamples::Resamples<A>::next bounds=2 unwrap=0 overflow=0 other=1
...
```

| column | what is counted |
|---|---|
| `bounds` | `panic_bounds_check`, `core::slice::index::*`, `str` slicing failures |
| `unwrap` | `Option`/`Result` `unwrap_failed`, `expect_failed` |
| `overflow` | `panic_const_*`: arithmetic overflow, division by zero, shift overflow |
| `other` | any other panic |

For Zig the kind comes from the name of the safety check: `outOfBounds` and `startGreaterThanEnd`
are `bounds`, `unwrapNull` and `unwrapError` are `unwrap`, `integerOverflow`, shift checks,
`divideByZero` and truncating casts are `overflow`, the rest (`reachedUnreachable`, `@panic`) is `other`.
Zig emits these checks only in `Debug` and `ReleaseSafe`, in `ReleaseFast` there is nothing to find.

Not counted: `std::thread::panicking()` checks (`panic_count`), compiler-inserted
`panic_in_cleanup`, and calls through a function pointer.

`--deny` makes it a CI check, it exits with code 1 when any function under the filter calls a panic:

```sh
copyhound panics --deny --filter probemap bench.ll || echo "a panic got into probemap"
```

`calls` lists what each function still calls directly, so you can see what did not get inlined
(LLVM intrinsics like `llvm.memcpy` are skipped):

```
$ copyhound calls --filter insert_inner bench.ll
probemap::SwissTable<E,S,A>::insert_inner
    1 probemap::SwissTable<E,S,A>::grow_and_insert
    1 probemap::fix_insert_slot_small
probemap::SwissTable<E,S,A>::insert_inner
    2 __rustc::__rust_dealloc
    1 bcmp
    1 probemap::SwissTable<E,S,A>::grow_and_insert
    1 probemap::fix_insert_slot_small
```

Calls through a pointer (`dyn Trait`, `&dyn Fn`, function pointers) show up as `<indirect>`.
`indirect` prints only those, one line per function with the count:

```
$ copyhound indirect --filter criterion bench.ll | sort -k2 -n -r | head -3
core::ptr::drop_in_place<criterion::connection::MessageError> 3
criterion::routine::Routine::profile 2
criterion::routine::Routine::profile 2
```

`diff` compares two IR files, for example before and after a change. Functions are matched by
demangled name, generic instances with the same name are summed:

```
$ copyhound diff --bytes 0 --filter probemap before.ll after.ll
changed core::ptr::drop_in_place<probemap::SwissTable<bench::PairExt>> lines 43 -> 30 copies 0 -> 0 panics 1 -> 0 indirect 0 -> 0
changed probemap::SwissTable<E,S,A>::insert_inner lines 434 -> 434 copies 3 -> 2 panics 0 -> 0 indirect 0 -> 0
changed probemap::SwissTable<E,S,A>::reserve_rehash lines 632 -> 610 copies 3 -> 3 panics 2 -> 0 indirect 0 -> 0
...
```

Lines start with `changed`, `added` or `removed`. `copies` counts `memcpy`, `memmove` and `memset`
above `--bytes`. When you build the two versions from `git archive` or two checkouts, use a separate
`CARGO_TARGET_DIR` for each, otherwise cargo can keep the old `.ll` because the extracted files have
old timestamps.

## Notes

- Rust names are demangled (`probemap::SwissTable<E,S,A>::insert_inner` instead of `_ZN8probemap...`). Zig names are already readable, only comptime arguments and instance numbers are stripped (`Io.Writer.printIntAny__anon_8323` becomes `Io.Writer.printIntAny`), so `diff` matches them between builds
- `--filter` matches the demangled name, use it instead of `grep` with `--context` because the output has many lines per hit
- copies with a size only known at run time are skipped by `memcpy`
