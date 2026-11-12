# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.1](https://github.com/codyps/local-rcu/compare/v1.1.0...v1.1.1) - 2026-11-12

### Other

- write_nosync() no longer looks at readers; one scan covers a batch ([#17](https://github.com/codyps/local-rcu/pull/17))
- keep reader epochs and the value pointer on cache lines of their own

## [1.1.0](https://github.com/codyps/local-rcu/compare/v1.0.8...v1.1.0) - 2026-11-12

### Added

- add cached::CachedReader, a reader that stays in its read section ([#15](https://github.com/codyps/local-rcu/pull/15))

### Other

- Fix typo in README.md regarding Writers
- move crate comparisons to COMPARISONS.md and fill them in ([#14](https://github.com/codyps/local-rcu/pull/14))
- Update README with details on reader and writer behavior
- Update terminology from 'destruction' to 'collection'
- Update README with details on reader behavior

## [1.0.8](https://github.com/codyps/local-rcu/compare/v1.0.7...v1.0.8) - 2026-11-11

### Fixed

- rely on the SeqCst fence pair, not SeqCst on `active`; epoch wrap

### Other

- describe the SeqCst protocol in the README
- make the commit labels clickable

## [1.0.7](https://github.com/codyps/local-rcu/compare/v1.0.6...v1.0.7) - 2026-11-11

### Fixed

- fix UB: make read sections always observe the current value (SeqCst active)

### Other

- record the runner hardware with each result
- stop charts growing without bound
- *(bench-history)* expand runner.temp via expression in upload path
- *(bench-history)* deploy Pages via actions, attach bench summary as git notes
- *(bench-history)* generate the site outside the worktree before wiping it
- record benchmarks per commit, publish history site on Pages
- run the test suite under miri on nightly
- add benchmark-comparison workflow (x86_64 + aarch64)
- Enhance README with epoch management details
- Update README with Writer epoch handling details
- Expand README with internal flow explanation

## [1.0.6](https://github.com/codyps/local-rcu/compare/v1.0.5...v1.0.6) - 2026-11-10

### Other

- publish to crates.io via trusted publishing ([#10](https://github.com/codyps/local-rcu/pull/10))
- update repo urls after the jmesmon -> codyps move ([#9](https://github.com/codyps/local-rcu/pull/9))
- document and test that the final value may be missed
- add opt-in writer-drop detection (local_rcu::closing)
- add ReadGuard::map / try_map (MappedReadGuard)

## [1.0.5](https://github.com/codyps/local-rcu/compare/v1.0.4...v1.0.5) - 2026-11-10

### Fixed

- drop accidentally committed declaration of the in-progress isolated bench
- release-plz: avoid updating changelog in repo
- switch to crane from naersk, try to coax sccache into running

### Other

- open the release PR as a GitHub App when configured ([#7](https://github.com/codyps/local-rcu/pull/7))
- enable changelog generation in release-plz ([#6](https://github.com/codyps/local-rcu/pull/6))
- Eliminate per-write Arc refcount traffic in reader tracking ([#5](https://github.com/codyps/local-rcu/pull/5))
- add local-rcu-left-right: the README recipe as a crate, benchmarked against left-right
- enable event-listener's loom feature in loom builds
- add runnable examples for the 'behavior like X' recipes
- bump criterion from 0.5.1 to 0.8.2 ([#4](https://github.com/codyps/local-rcu/pull/4))
- refresh the alternatives list
- modernize workflow, dependabot; fix clippy lints
- validate compile-fail stderr only in the loom job, quiet flakehub warnings
- don't fail-fast, skip compile-fail test on beta
- bump MSRV matrix entry to 1.88, fix lifetime elision warning
- pin action versions, drop sccache, bump MSRV to 1.85
- use main as default branch
- update cargo and nix dependencies
- keep compile-test stderr
- docs
- smarter yield
- name threads
- docs
- update tests/compile-fail for compiler update
- emit cfg loom usage
- nix flake update
- add iconv for darwin
- nix flake update
- try using release-plz
- make trybuild work regardless of rust source code location
