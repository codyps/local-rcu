# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
