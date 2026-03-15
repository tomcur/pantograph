# Pantograph

<img align="right" width="20%" src="logo.svg">

[![dependency status](https://deps.rs/repo/github/tomcur/pantograph/status.svg)](https://deps.rs/repo/github/tomcur/pantograph)
[![Apache 2.0 or MIT license.](https://img.shields.io/badge/license-Apache--2.0_OR_MIT-blue.svg)](#license)
[![Build status](https://github.com/tomcur/pantograph/workflows/CI/badge.svg)](https://github.com/tomcur/pantograph/actions)
[![Crates.io](https://img.shields.io/crates/v/pantograph.svg)](https://crates.io/crates/pantograph)
[![Docs](https://docs.rs/pantograph/badge.svg)](https://docs.rs/pantograph)

<!-- We use cargo-rdme to update the README with the contents of lib.rs.
To edit the following section, update it in lib.rs, then run:
cargo rdme --readme-path=README.md --workspace-project=pantograph --heading-base-level=0
Full documentation at https://github.com/orium/cargo-rdme -->

<!-- cargo-rdme start -->

`pantograph` provides channels for sending payloads across threads, where at least one side of
the channel is *wait-free*.

Synchronization being "wait-free" is a stronger guarantee than "lock-free." Lock-free
guarantees the full system makes progress and cannot get in a deadlock; wait-free additionally
guarantees per-thread progress. Wait-free channels are useful for problems that must meet
particular timing requirements, but they are not necessarily faster than lock-free channels.

The channels are organized into three groups.
- In `lf_wf`, producer(s) are lock-free, consumer(s) are wait-free.
- In `wf_lf`, producer(s) are wait-free, consumer(s) are lock-free.
- In [`wf_wf`], producer(s) and consumer(s) are wait-free.

<!-- cargo-rdme end -->

## Minimum supported Rust Version (MSRV)

This version of Pantograph has been verified to compile with **Rust 1.85** and later.

Future versions of Pantograph might increase the Rust version requirement.
It will not be treated as a breaking change and as such can even happen with small patch releases.

<details>
<summary>Click here if compiling fails.</summary>

As time has passed, some of Pantograph's dependencies could have released versions with a higher Rust requirement.
If you encounter a compilation issue due to a dependency and don't want to upgrade your Rust toolchain, then you could downgrade the dependency.

```sh
# Use the problematic dependency's name and version
cargo update -p package_name --precise 0.1.1
```
</details>

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Contribution

Contributions are welcome by pull request. The [Rust code of conduct] applies.
Please feel free to add your name to the [AUTHORS] file in any substantive pull request.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be licensed as above, without any additional terms or conditions.

[Rust Code of Conduct]: https://www.rust-lang.org/policies/code-of-conduct
[AUTHORS]: ./AUTHORS
