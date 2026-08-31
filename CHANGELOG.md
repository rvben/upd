# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.8](https://github.com/rvben/upd/compare/v0.8.7...v0.8.8) - 2026-08-31

### Fixed

- **cache**: isolate Python index chains and revalidate cached releases older than the manifest

## [0.8.7](https://github.com/rvben/upd/compare/v0.8.6...v0.8.7) - 2026-08-31

### Added

- **actions**: make dependency PRs email-safe ([33275fd](https://github.com/rvben/upd/commit/33275fdaad1cac619ed1b6176a58834835c66662))

## [0.8.6](https://github.com/rvben/upd/compare/v0.8.5...v0.8.6) - 2026-08-30

### Fixed

- **automation**: clarify blocked dependency guidance ([771db8b](https://github.com/rvben/upd/commit/771db8b52068ef2e4c302c8e916f121fa30d98cb))
- **actions**: pin self-hosted reusable workflows ([2f18db0](https://github.com/rvben/upd/commit/2f18db0ad2001c77b8b553c6abb55cf553162450))

## [0.8.5](https://github.com/rvben/upd/compare/v0.8.4...v0.8.5) - 2026-08-30

### Added

- **config**: add a versioned opt-in policy for scheduled security remediation ([5513ede](https://github.com/rvben/upd/commit/5513edefc15ec6a9425edc90868902f312deeb18))

### Changed

- **actions**: embed the hosted broker endpoint in reusable workflows ([1d9075c](https://github.com/rvben/upd/commit/1d9075c8c5c2b15242cecc85367edbdebc7d7178))
- **actions**: replace caller-held GitHub App keys with the OIDC-authenticated hosted token broker ([355b6dc](https://github.com/rvben/upd/commit/355b6dc049f271e8ca9157f1d6595470d4c49066))

## [0.8.4](https://github.com/rvben/upd/compare/v0.8.3...v0.8.4) - 2026-08-29

### Added

- **automation**: deliver review-ready dependency proposals ([849e945](https://github.com/rvben/upd/commit/849e9459c9a4ce475d4fb5ecb62fba281fcaa117))
- **automation**: improve dependency update reviews ([84109ea](https://github.com/rvben/upd/commit/84109eaf36c739dc11af0452c6218abb7e47a8e3))
- **actions**: improve security remediation reviews ([d4830e9](https://github.com/rvben/upd/commit/d4830e9a8b6dca102fd2d30147b387b39e30bf11))
- **brand**: add GitHub App badge ([1d0d2ca](https://github.com/rvben/upd/commit/1d0d2ca880ba15c9db903a3da28ecf1aa2c01b2a))
- **actions**: add secure dependency remediation ([ddda7ee](https://github.com/rvben/upd/commit/ddda7eeff343877ff18214f953af35f20b0d56c6))

### Fixed

- **update**: read a spaced bound as the bound it is ([918def0](https://github.com/rvben/upd/commit/918def0fee12c78f87a11d84ad74e936f3c71784))
- **actions**: use App credentials for git pushes ([04b96d4](https://github.com/rvben/upd/commit/04b96d425833bb3ab3bd134b78256e7e8ac3a67c))
- **actions**: authenticate remediation publishing ([94b5046](https://github.com/rvben/upd/commit/94b504618ad6cd5eb9cf3558bfa0f6593666ea41))
- **actions**: generate valid remediation PR body ([c4d663a](https://github.com/rvben/upd/commit/c4d663a21961188f4ef7962416cecd519b4df69b))
- **deps**: update vulnerable Rust dependencies ([2270522](https://github.com/rvben/upd/commit/2270522ec7fb0f6abc6f2bf831a260a402990eb2))

## [0.8.3](https://github.com/rvben/upd/compare/v0.8.2...v0.8.3) - 2026-08-27

## [0.8.2](https://github.com/rvben/upd/compare/v0.8.1...v0.8.2) - 2026-08-27

### Fixed

- **actions**: reach a workflow's annotations without also selecting `actions` ([a14fa14](https://github.com/rvben/upd/commit/a14fa14191815894ef5e7cc09531ffe152928a7d))

## [0.8.1](https://github.com/rvben/upd/compare/v0.8.0...v0.8.1) - 2026-08-27

### Added

- **actions**: update annotated tool versions inside workflows ([9fb5951](https://github.com/rvben/upd/commit/9fb5951a252e7632ed6c152b45647461cb22c697))

## [0.8.0](https://github.com/rvben/upd/compare/v0.7.1...v0.8.0) - 2026-08-26

### Added

- **npm**: update hyphen, wildcard and comparator ranges ([6548401](https://github.com/rvben/upd/commit/65484016d0d3d32a05bee5ad9709d71e3e82a880))

### Fixed

- **terraform**: reject unreadable version constraints ([d0c2be3](https://github.com/rvben/upd/commit/d0c2be3cde56d1ee375e7fe2bb8f678439de0bbf))
- **update**: read the highest of several lower bounds as the floor ([2a4a319](https://github.com/rvben/upd/commit/2a4a3199574495f606265e73b8fef639e3e22bd3))
- **npm**: report a pin the range cannot hold instead of writing it ([c9cfce8](https://github.com/rvben/upd/commit/c9cfce820964170d164c6c56813c19d8469ab8e8))
- **rubygems**: read a hyphen as the prerelease marker Gem::Version writes ([b575928](https://github.com/rvben/upd/commit/b575928d29d559189dab9a620633b1358994095f))
- **interactive**: write a floor that follows a ceiling ([fa6666c](https://github.com/rvben/upd/commit/fa6666cd9a0344abd8df1cccfc440195f5321c90))
- **npm**: treat a bound written over a wildcard as floorless ([4eb0196](https://github.com/rvben/upd/commit/4eb0196781b3450e4b6b0b2bc9b6ab11abcd4d2e))
- **nuget**: order pre-release identifiers the way NuGet does ([1b2641e](https://github.com/rvben/upd/commit/1b2641eeb09e96087d36a9ce36b52184391b734a))
- **interactive**: report a scan error in the exit code ([c8b47be](https://github.com/rvben/upd/commit/c8b47bef31398bdd2a7901ef80e40a494eb87d51))
- **npm**: report a pin that has no floor to raise instead of writing it ([3e3f833](https://github.com/rvben/upd/commit/3e3f833acfcde1f3a2161f120e3aa2ab13ea1bfc))
- **terraform**: compare versions the way the registry does ([987993f](https://github.com/rvben/upd/commit/987993fe090d31ff4cb416bb3b8d19ccb1cf911b))
- **rubygems**: compare versions the way Gem::Version does ([10cc2a7](https://github.com/rvben/upd/commit/10cc2a7e4a5fb83007fb6e812622c06c09e23420))
- **npm**: read the tilde npm spells with a trailing arrow ([cfe682c](https://github.com/rvben/upd/commit/cfe682c89f799af9ea0b3170bb0fd4095d85b733))
- **update**: look each declaration up at its own requirement ([0f99439](https://github.com/rvben/upd/commit/0f994395e4546b6ec82b6c7c116bf97e9d8a01a1))
- **schema**: declare every error kind and what exit 2 covers ([9236d5c](https://github.com/rvben/upd/commit/9236d5c7e3f7aa3bbef83503cc8c5b8e0f1ef9ed))
- **nuget**: report a version range instead of skipping it silently ([e65cac5](https://github.com/rvben/upd/commit/e65cac597e32cd6583a16a9712eb6b76eeada86e))
- **update**: leave a bound that is not a floor alone and report it ([4b607ab](https://github.com/rvben/upd/commit/4b607ab8c726ba1a046c37940ac05ddaabcac26e))
- **output**: withhold the up to date tick while a warning stands ([44c1162](https://github.com/rvben/upd/commit/44c1162d218c2a544c8352e0c4cfdba97698d334))
- **update**: refuse to raise a bound that is not a floor ([6e2a1c2](https://github.com/rvben/upd/commit/6e2a1c2007caa3d609c6aa03a570f25500b29d42))

## [0.7.1](https://github.com/rvben/upd/compare/v0.7.0...v0.7.1) - 2026-08-25

### Fixed

- **github-releases**: fall back to tags when the latest release is not a version ([6e87286](https://github.com/rvben/upd/commit/6e8728684c64891debd4595f1633b62079f8abb9))

## [0.7.0](https://github.com/rvben/upd/compare/v0.6.5...v0.7.0) - 2026-08-25

### Added

- **discovery**: include annotated files by glob ([1163ef1](https://github.com/rvben/upd/commit/1163ef17f1a8b1b308a749f14099171200dce2c0))

### Fixed

- **interactive**: withhold the up-to-date tick from unchecked pins ([38e0584](https://github.com/rvben/upd/commit/38e0584c1c780e277374d1ae682d37100b9eb5f5))

## [0.6.5](https://github.com/rvben/upd/compare/v0.6.4...v0.6.5) - 2026-08-25

### Added

- **cli**: add package filter shorthand ([9dd6c8d](https://github.com/rvben/upd/commit/9dd6c8d5087021795382830d313e4b56a58fef68))

### Fixed

- **ci**: support GitHub App update credentials ([cc30410](https://github.com/rvben/upd/commit/cc304104b309cb04e5e0682040ff487e62a13047))
- **actions**: recover releases for bare SHA pins ([e9eb595](https://github.com/rvben/upd/commit/e9eb59599d1dd5f6670a716b9e90760429673c0b))
- **release**: bootstrap cargo tools from binaries ([9f8bf3b](https://github.com/rvben/upd/commit/9f8bf3b10804beef0396eff67b691bc62d5e7332))
- **toolchain**: align Rust version declarations ([9789051](https://github.com/rvben/upd/commit/9789051f401f2146476991640a4884d59c1483cd))
- **ci**: isolate mise tool installation ([b5299ef](https://github.com/rvben/upd/commit/b5299efba219bbf62fa9814074029c35d7942075))
- **release**: remove workflow permission from pin sync ([bc8f707](https://github.com/rvben/upd/commit/bc8f70776d338db91d19cbaea2429dcf69d2e052))
- **release**: isolate pin validation tooling ([0181c26](https://github.com/rvben/upd/commit/0181c261aa5ad5d61d2e170a1d88da0156a8a81a))
- **release**: install Rust components for pin validation ([c14b1dc](https://github.com/rvben/upd/commit/c14b1dc4f8953fcfa3b88a997ec36a5daead8137))

### Performance

- **release**: install maturin from attested binaries ([21e7f31](https://github.com/rvben/upd/commit/21e7f31c85bec5e43f0abfa4742d231423067fe1))
- **release**: scope cross tools to Linux ([4f20409](https://github.com/rvben/upd/commit/4f2040985eb4ab39ca69885412473ed1e513f4f5))

## [0.6.4](https://github.com/rvben/upd/compare/v0.6.3...v0.6.4) - 2026-08-24

### Added

- **release**: automate verified integration pin updates ([8bfc014](https://github.com/rvben/upd/commit/8bfc014bbcf9b3ac13d2f33225be83d32d6bf023))
- **packaging**: publish Python distribution as upd ([1fb9547](https://github.com/rvben/upd/commit/1fb9547ee40f22e69ec10f6f5502ab70aaa3807b))
- **ci**: add GitHub and GitLab dependency automation ([096a05e](https://github.com/rvben/upd/commit/096a05eec5def0d22e6f3455151c25f2ef64df61))

### Fixed

- **lock**: scope direct edges to the current package ([7daaf08](https://github.com/rvben/upd/commit/7daaf08293f8a80e72f3248585b7e20ef48d0999))
- **lock**: anchor an ambiguous cargo spec on the lockfile's direct edge ([46fa2de](https://github.com/rvben/upd/commit/46fa2de19cae9bfa91213efc46c46f7b61ea3502))
- **lock**: qualify an ambiguous cargo package spec with its locked version ([e84868c](https://github.com/rvben/upd/commit/e84868c91fc2c2ad998e929f16996bdea1402b1b))

## [0.6.3](https://github.com/rvben/upd/compare/v0.6.2...v0.6.3) - 2026-08-24

### Fixed

- **update**: anchor a multi-clause version range on its lower bound ([9b1719c](https://github.com/rvben/upd/commit/9b1719ccadf6939b9db35849d723135125b7d0c0))
- **update**: report a floor diagnostic once per manifest that wants it ([7d89e71](https://github.com/rvben/upd/commit/7d89e7143eb31f371b2448dc37e7e9f05a4e565d))
- **update**: count a floor upd was told not to write ([7167892](https://github.com/rvben/upd/commit/71678922a43e4f951b66a1de53b4e2c3d66a84b0))
- **update**: read a lock-only floor's ignore rule from each project's own config ([a25b974](https://github.com/rvben/upd/commit/a25b974b4992928175ac431eb5c2eb08a0eb3bdf))
- **update**: count an unwritable floor and never report it as held back ([bfd6dcd](https://github.com/rvben/upd/commit/bfd6dcd43af420aa97bcb44eb101dcdb0aad22d4))
- **update**: report a lock-only floor above the ceiling as held back ([d72b272](https://github.com/rvben/upd/commit/d72b272056cea221409574b1b61fa1fba60c12c1))
- **bump**: read the bump level from a range spec's lower bound ([e79cb64](https://github.com/rvben/upd/commit/e79cb6404d4f7965ff5081a0dc86dca16e040684))
- **bump**: classify a step between zero-major versions as breaking ([2ec6bf5](https://github.com/rvben/upd/commit/2ec6bf558dded08706227bd23932e7f9c4fc8c1a))
- **bump**: report updates held back by the bump ceiling ([7f83977](https://github.com/rvben/upd/commit/7f83977f0a75b681be54ea680282666668406ebc))

## [0.6.2](https://github.com/rvben/upd/compare/v0.6.1...v0.6.2) - 2026-08-21

### Fixed

- **cooldown**: distinguish a failed publish-date lookup from a registry without dates ([447bee0](https://github.com/rvben/upd/commit/447bee039cbc9304006e13085e5efbba89e4bb7a))
- **github-actions**: report a failed ref lookup instead of reading it as no refs ([9916f74](https://github.com/rvben/upd/commit/9916f74be996b03bad5f1cf960fa118817ee3f81))

## [0.6.1](https://github.com/rvben/upd/compare/v0.6.0...v0.6.1) - 2026-08-21

### Fixed

- **actions**: resolve version comments that omit the v prefix ([0db7fcc](https://github.com/rvben/upd/commit/0db7fcc2b7c8867f45c64e991a3182ca05210ad1))
- **cooldown**: report an unknown publish date instead of the current time ([e0c552e](https://github.com/rvben/upd/commit/e0c552ef4c66b0323eb4cd2424cbf10558d0b972))
- **go**: resolve the newest Go module version instead of an arbitrary one ([5e3df0e](https://github.com/rvben/upd/commit/5e3df0ea434472b043b38b41fa71d09b010e6643))
- **config**: report the resolved configuration from --show-config ([a80fea0](https://github.com/rvben/upd/commit/a80fea03b9c49a16a687a4f7da46613ffb7497b2))

## [0.6.0](https://github.com/rvben/upd/compare/v0.5.4...v0.6.0) - 2026-08-18

### Added

- check GitHub Actions SHA pins by default ([f9da91e](https://github.com/rvben/upd/commit/f9da91eefd7d33457ccbaba8605a8be7822e14b8))
- apply GitHub Actions SHA pin updates in interactive mode ([a8836dd](https://github.com/rvben/upd/commit/a8836dda20805fe36654af52d76d32e46c62742f))

## [0.5.4](https://github.com/rvben/upd/compare/v0.5.3...v0.5.4) - 2026-08-18

### Fixed

- **actions**: report SHA-pinned actions left unchecked instead of dropping them ([31fbf34](https://github.com/rvben/upd/commit/31fbf342651eb5dab40aff73a6d6301e7a47f9d3))
- **requirements**: keep the default index when only --extra-index-url is set ([26388b2](https://github.com/rvben/upd/commit/26388b28f11674ae00a6b8d9ac8acbde8edca3da))
- **output**: do not report "up to date" when lookups failed ([ca555f8](https://github.com/rvben/upd/commit/ca555f8ab4f018da363f0ae53f98999d33a69812))
- **pyproject**: consult declared indexes alongside the default index ([edd7922](https://github.com/rvben/upd/commit/edd7922cae34e5f70bce4b5848e7b58aaa01579c))

## [0.5.3](https://github.com/rvben/upd/compare/v0.5.2...v0.5.3) - 2026-08-15

### Fixed

- **cooldown**: ignore already-current versions ([140baff](https://github.com/rvben/upd/commit/140baff6dc3c7a6f768743671c8d261c8d1097be))

## Unreleased

## [0.5.2](https://github.com/rvben/upd/compare/v0.5.1...v0.5.2) - 2026-08-15

### Fixed

- **actions**: support current hosted runner labels ([79f3325](https://github.com/rvben/upd/commit/79f3325f619c777a0b2c83d34863803be607f41d))

## [0.5.1](https://github.com/rvben/upd/compare/v0.5.0...v0.5.1) - 2026-08-15

### Fixed

- **actions**: scope actionlint to workflow syntax ([3e1b53e](https://github.com/rvben/upd/commit/3e1b53eb2c7d7f5de1680c0a6f3a09f2d4944df9))

## [0.5.0](https://github.com/rvben/upd/compare/v0.4.1...v0.5.0) - 2026-08-14

### Added

- **actions**: update verified SHA pins ([e3c5118](https://github.com/rvben/upd/commit/e3c51188a8cd29831e0c51913b8d6cf765a4c5d7))

## [0.4.0](https://github.com/rvben/upd/compare/v0.3.1...v0.4.0) - 2026-08-12

### Added

- **discovery**: find annotated version pins in Makefiles and shell scripts ([549ee2a](https://github.com/rvben/upd/commit/549ee2a54706264ac58bb36d87ad35b9d7577a2e))
- **updater**: update dependencies declared by comment annotation ([d4736d0](https://github.com/rvben/upd/commit/d4736d044e0157bc137e3afa936a26705334a03b))
- **updater**: add the annotated registry set and lang carrier ([12cd2f2](https://github.com/rvben/upd/commit/12cd2f29406462c13547a2f6d0f0a6f8fde4cda4))
- **annotation**: locate, rewrite and classify version tokens ([a4ff1f3](https://github.com/rvben/upd/commit/a4ff1f383eb75127a76489c7723bd6fb6a932a17))
- **annotation**: parse upd: and renovate: version annotations ([bd973f1](https://github.com/rvben/upd/commit/bd973f1311dbbd5d71f5cb3208f3790f31a00ef3))
- **lang**: add github-releases and annotated ecosystems ([a0ff07d](https://github.com/rvben/upd/commit/a0ff07dc84f0b95ca374ab38ed74a1d851d4f688))

### Fixed

- **annotated**: preserve files and harden parsing ([993de3a](https://github.com/rvben/upd/commit/993de3a442d5611516a7901da1efff74e4a4c5ed))
- **interactive**: surface per-file warnings and errors under --interactive ([97373d2](https://github.com/rvben/upd/commit/97373d291626a2c4f103b65697b147485e17fd25))

## [0.3.1](https://github.com/rvben/upd/compare/v0.3.0...v0.3.1) - 2026-07-22

### Fixed

- **actions**: only shorten a version to a ref the repo actually publishes ([3321fbe](https://github.com/rvben/upd/commit/3321fbe3f5997bcbd51b2c58cce80edf7d620a79))

## [0.3.0](https://github.com/rvben/upd/compare/v0.2.4...v0.3.0) - 2026-07-18

### Added

- **update**: floor lock-only packages named by --package to the registry latest ([4747adf](https://github.com/rvben/upd/commit/4747adf863e8122b116719053b534976669bb067))
- **audit**: fix lock-only findings with transactional floors, implied --lock, and structured fixes reporting ([0c42c33](https://github.com/rvben/upd/commit/0c42c33fe109fff09e315b380a643959714a72c4))
- **fix**: apply fix targets in transactional groups with lockfile snapshots and rollback ([e2d2814](https://github.com/rvben/upd/commit/e2d28140691b5292e54d302c99c439c91b8ce8da))
- **fix**: write npm override floors with EOVERRIDE-aware forms and never-weaken protection ([7549351](https://github.com/rvben/upd/commit/754935132e1628f8ce87e743002fe7948bc02493))
- **fix**: write uv constraint-dependencies floors with never-weaken protection ([8c25195](https://github.com/rvben/upd/commit/8c25195a846317c6464e94fbeef2c985dd7940f0))
- **fix**: route vulnerable pairs into explicit manifest-edit and floor targets ([b6877b9](https://github.com/rvben/upd/commit/b6877b946e5c7b13fd58b80da439eb9a437b6a24))
- **lockscan**: classify positional provenance of locked packages per (name, version) pair ([2024883](https://github.com/rvben/upd/commit/2024883760d1f7d834b72beadb93859363c03b24))
- **lockscan**: associate workspace member manifests with their nearest scannable lock ([416d95e](https://github.com/rvben/upd/commit/416d95e9b47d628f77e7c0810f815c0ec21c5198))
- **lockscan**: record npm entry locators and parse direct-dep declarations for provenance ([4a4b754](https://github.com/rvben/upd/commit/4a4b7546ef65e95ce79a8be751a41aaf68841e2f))
- **audit**: scan lockfiles so lock-only transitive dependencies are audited ([d3237da](https://github.com/rvben/upd/commit/d3237da8aa0cbd967ceddef060c01b5c4d99999a))
- **lockscan**: discover scannable lockfiles with workspace and coverage guards ([06b8757](https://github.com/rvben/upd/commit/06b875792188fe47a520f5b734ebf06ee71740ce))
- **lockfile**: treat npm-shrinkwrap.json as authoritative and add --ignore-scripts to npm relock ([0e11f31](https://github.com/rvben/upd/commit/0e11f31ec1ed14c78577ed2d0a9d3f68ddf76395))
- **lockscan**: add package-lock.json reader with alias, scope, and legacy-version handling ([4fd2c0a](https://github.com/rvben/upd/commit/4fd2c0a642679332523d8c0f1703e76ecfcdbfa1))
- **lockscan**: add poetry.lock and Cargo.lock readers ([5f66fca](https://github.com/rvben/upd/commit/5f66fca7d05afc19be95360f4d6b6fee92357d2a))
- **lockscan**: add parse-only lockfile scanning module with uv.lock reader ([d7e9254](https://github.com/rvben/upd/commit/d7e925485087724739142e43579835b42fcc4011))
- **audit**: add coverage warnings channel and go.mod pre-1.17 warning ([597cba7](https://github.com/rvben/upd/commit/597cba7fc02662a1858f16e69cecaadf4e639f0f))
- **audit**: show CVE aliases and advisory source in text, JSON, and schema docs ([725f0ee](https://github.com/rvben/upd/commit/725f0ee8fcd6d81f52dc375c57eca4c46906cb22))
- **audit**: version the audit cache and normalize PyPI cache keys per PEP 503 ([119bc04](https://github.com/rvben/upd/commit/119bc04ac3f347b6548c971a389ee752e5879eb1))
- **audit**: carry advisory aliases and source database on findings ([5eaddf2](https://github.com/rvben/upd/commit/5eaddf2cec6802817ee4a98953db09af8fc9b5c6))
- **audit**: add PEP 503 package-name normalization helper ([9ff5488](https://github.com/rvben/upd/commit/9ff5488553b2638bd8d5c60a29655fcd8cd2a119))

### Fixed

- **fix**: skip cargo-precise floors under --no-lock in dry-run previews too ([e89d3df](https://github.com/rvben/upd/commit/e89d3df39d4c0609721c7744a9135f149412cb2b))
- **update**: count only planned and applied floors in the update summary ([2a2080b](https://github.com/rvben/upd/commit/2a2080b88a23ea51fde4f0b552ab16af7877a7eb))
- **audit**: report apply-time unfixable floors in text mode and correct no-lock edit wording ([bee76a1](https://github.com/rvben/upd/commit/bee76a1ca2a0fb90a4b8d3885c9ee82d2d86d899))
- **fix**: attribute manifest edits per owner and merge all edits in one line-aware pool ([653ff4e](https://github.com/rvben/upd/commit/653ff4e661aca18c2a8a84e330372bc7cd8891e9))
- **lockscan**: keep per-lockfile provenance entries so independent projects never shadow each other ([2e66da0](https://github.com/rvben/upd/commit/2e66da0be6492495fc0fa3af9f07113026815689))
- **audit**: distinguish unbumpable manifest pins from lock-only packages in fix-audit diagnostics ([edf183f](https://github.com/rvben/upd/commit/edf183fd2ba3074d5b1ba4c0794c33b32557e510))
- **audit**: report lock-only packages that --fix-audit cannot edit ([5df74ac](https://github.com/rvben/upd/commit/5df74ac9c44916394d63b1dcc13d9292c428da61))
- **lockscan**: do not follow symlinks in workspace membership walk ([0d10b6c](https://github.com/rvben/upd/commit/0d10b6ceb5d71bbbaf5fb3f646aea549ea879121))
- **audit**: parse renamed Cargo dependencies under their real package name ([5ce219d](https://github.com/rvben/upd/commit/5ce219d8662fdf74556c1e604a3dc7fc2b50c5db))
- **lockscan**: anchor entry lines to keys at line start, not references ([2d49a1e](https://github.com/rvben/upd/commit/2d49a1ee85f6193151c69d087aca9847041205ee))
- **audit**: scope fixed-version extraction to the queried package and version branch ([c0fcf10](https://github.com/rvben/upd/commit/c0fcf107a834168b1f7683d4f8892fbe0ab7ab21))

### Performance

- **lockscan**: index lockfile entry lines in a single pass ([845bda5](https://github.com/rvben/upd/commit/845bda548ac32fbd9f752fd6f9a629b8e068c3cd))

## [0.2.4](https://github.com/rvben/upd/compare/v0.2.3...v0.2.4) - 2026-07-09

### Added

- **audit**: honor --lock in audit --fix-audit --apply ([cd48a6e](https://github.com/rvben/upd/commit/cd48a6eba70aac563eef2e2d93180e0c77ab65ec))

### Fixed

- **audit**: keep the v prefix when writing Go fix versions to go.mod ([607ddc1](https://github.com/rvben/upd/commit/607ddc14dd48ed7418294f6b4e7a5a2041379fa6))

## [0.2.3](https://github.com/rvben/upd/compare/v0.2.2...v0.2.3) - 2026-06-24

### Fixed

- **config**: hard-fail on a malformed config instead of using defaults ([6e892bc](https://github.com/rvben/upd/commit/6e892bc45c70609f7a07f3502b3f329ef13434c0))
- **go**: warn on a go.mod with no module directive ([c71e3ea](https://github.com/rvben/upd/commit/c71e3ea667144fe7ae5d4cedbf581a97672bffc0))
- **update**: preserve original BOM and line endings on write ([64a6a99](https://github.com/rvben/upd/commit/64a6a99c603b4049f37fce11beda270dfec7f767))
- **mise**: resolve golang/go releases by parsing go-prefixed tags ([09f0dc1](https://github.com/rvben/upd/commit/09f0dc1d84a7a696235353749b9aa21dd59584fd))
- **cli**: suggest the closest subcommand for a mistyped positional ([3a77e96](https://github.com/rvben/upd/commit/3a77e962fe098c0a68c98f4806f39b1a8a107ce7))
- **cli**: thread bump filter into writes, fix verbose JSON, signal align dry-run ([a53166f](https://github.com/rvben/upd/commit/a53166fa3a0818f56645e3cde71b49cb148182f0))
- **update**: add write-time bump gate, version bounds, and file safety ([092ccf3](https://github.com/rvben/upd/commit/092ccf366ba18402d498b00c7939f707869899c3))
- **schema**: correct audit/align output_fields and add arg enums ([543b02b](https://github.com/rvben/upd/commit/543b02bdcb9b78b3f262795403fece1998242d19))
- **config**: match [pin] package names case-insensitively ([1dd55c7](https://github.com/rvben/upd/commit/1dd55c7cce72b5c6596b0572f3a11f52ea4cb0e6))
- **audit**: never surface a Git commit SHA as the fixed version ([1e0d580](https://github.com/rvben/upd/commit/1e0d5800b44f102d25e425e23066aeb9234178fa))

## [0.2.2](https://github.com/rvben/upd/compare/v0.2.1...v0.2.2) - 2026-06-23

### Added

- **align**: honor ignore and exclude config in align ([d856f3b](https://github.com/rvben/upd/commit/d856f3b351d569ba8cba5e368e73fc3bbe08a7f4))
- **config**: add exclude key and case-insensitive ignore matching ([95d40f5](https://github.com/rvben/upd/commit/95d40f5f5547438625336d90f35c7ecdea55cd62))

## [0.2.1](https://github.com/rvben/upd/compare/v0.2.0...v0.2.1) - 2026-06-11

### Fixed

- correct five scripted-consumer defects in CLI contract ([936687d](https://github.com/rvben/upd/commit/936687da01d2af39b884415a9e0262a8197490d7))

## [0.2.0](https://github.com/rvben/upd/compare/v0.1.10...v0.2.0) - 2026-06-11

### Breaking Changes

- **audit**: give vulnerabilities_found its own exit code 6 as a declared outcome ([0037dc4](https://github.com/rvben/upd/commit/0037dc4e075f7d3cca7e51096fc11d07e1aa1cdb))

### Added

- **audit**: give vulnerabilities_found its own exit code 6 as a declared outcome ([0037dc4](https://github.com/rvben/upd/commit/0037dc4e075f7d3cca7e51096fc11d07e1aa1cdb))

## [0.1.10](https://github.com/rvben/upd/compare/v0.1.9...v0.1.10) - 2026-06-11

### Added

- **schema**: declare updates_available as an outcome, not an error kind ([4f55a02](https://github.com/rvben/upd/commit/4f55a02e3b80384f19199691302f96ba4deb9246))

## [0.1.9](https://github.com/rvben/upd/compare/v0.1.8...v0.1.9) - 2026-06-11

### Added

- **clispec**: implement clispec v0.2 compliance (24/24) ([1c477cc](https://github.com/rvben/upd/commit/1c477cc11252dbad4f6d46fa7f393dcb48fb170d))

## [0.1.8](https://github.com/rvben/upd/compare/v0.1.7...v0.1.8) - 2026-04-30

### Added

- **packaging**: add upd-cli shim binary so `uvx upd-cli` works directly ([17fc581](https://github.com/rvben/upd/commit/17fc581015d2a44dcaa16a547597f4bfd0013751))

## [0.1.7](https://github.com/rvben/upd/compare/v0.1.6...v0.1.7) - 2026-04-29

### Fixed

- **tls**: switch reqwest backend from rustls to native-tls ([dd8375d](https://github.com/rvben/upd/commit/dd8375de58dc8c024476dbec4d171a2af56f7ebc))

## [0.1.6](https://github.com/rvben/upd/compare/v0.1.5...v0.1.6) - 2026-04-29

### Added

- **http**: wire TLS init into networked subcommand entry points ([04f7b0d](https://github.com/rvben/upd/commit/04f7b0d6362f32ff2a6212386f310b481bfd7d56))
- **cli**: add --insecure global flag ([df1943f](https://github.com/rvben/upd/commit/df1943ff53369b0cc0b32401cbbedb1095f07230))
- **http**: attach TLS hint to send errors via wrap_send_err ([9b1fd2c](https://github.com/rvben/upd/commit/9b1fd2cef7e307a6d3328074a82353a772a5b44c))
- **http**: apply TLS options to ClientBuilder ([7d8f616](https://github.com/rvben/upd/commit/7d8f6161f5f7be31e829f89ba9aaeb332532229f))
- **http**: implement init() over pure helpers ([79556c6](https://github.com/rvben/upd/commit/79556c6d5a34cf5eda97e645b4d62c62ad706809))
- **http**: detect TLS trust failures in error chains ([e63be5d](https://github.com/rvben/upd/commit/e63be5d62cfa82f2a73f3e78e294c65868c53ba7))
- **http**: parse PEM CA bundle with multi-cert support ([07e3983](https://github.com/rvben/upd/commit/07e3983a9477dad042ea0f5b10ee8efa9382b384))
- **http**: resolve CA bundle path from env vars ([2bd42ec](https://github.com/rvben/upd/commit/2bd42ec7018094e1daf399af8fb2e7e288e9e206))
- **http**: scaffold TLS options module ([3f797cc](https://github.com/rvben/upd/commit/3f797cc658298b9b952e566b49d4998a4decfc64))

### Fixed

- **http**: propagate --insecure global flag to self-update ([88b397e](https://github.com/rvben/upd/commit/88b397ee4a169eab94b56247050e8b1814c8ccf3))
- **http**: defer TLS init past offline and no-op early returns ([41cfb40](https://github.com/rvben/upd/commit/41cfb40f2f72e6090fc0f083c13371c3a4269347))
- **http**: skip CA bundle resolution when --insecure is set ([8d73e27](https://github.com/rvben/upd/commit/8d73e27aadaa5ac488bbbf7f556476cf9bfaa0e6))

## [0.1.5](https://github.com/rvben/upd/compare/v0.1.4...v0.1.5) - 2026-04-28

### Added

- **audit**: include package names and HTTP body in OSV error diagnostics ([87b9aa8](https://github.com/rvben/upd/commit/87b9aa8d27545abf449461c3c3d096a661fa377f))

## [0.1.4](https://github.com/rvben/upd/compare/v0.1.3...v0.1.4) - 2026-04-28

### Added

- **discovery**: respect .gitignore for hidden ecosystem files and add --no-ignore ([bd44269](https://github.com/rvben/upd/commit/bd442692e4e7c5ab96e1163128ac8f274a771f30))

## [0.1.3](https://github.com/rvben/upd/compare/v0.1.2...v0.1.3) - 2026-04-25

### Added

- **lock**: scope lockfile regeneration to the packages upd actually changed ([6b6cfa6](https://github.com/rvben/upd/commit/6b6cfa6fe3e8e8d787e78c7afea24883ebe67833))
- **npm**: classify and rewrite comparator-range specs ([a60979a](https://github.com/rvben/upd/commit/a60979acd7edb7eb6adfac554a590412a6cf8271))

### Fixed

- **lock**: include config pins in targeted regenerate and update CLI help ([207fdf9](https://github.com/rvben/upd/commit/207fdf94af95c74865f828f258ec9cfa61d22085))
- **npm**: preserve upper bound when pinning comparator-range specs ([fb7c863](https://github.com/rvben/upd/commit/fb7c863de9f07201af310ab13a59696d17fcb766))
- **npm**: apply config policy and cooldown to comparator-range updates ([be095dd](https://github.com/rvben/upd/commit/be095dd30443dd547fb6913dae551cbd530cb019))
- **npm**: update comparator-range specs via constraint-aware resolution ([4481b8b](https://github.com/rvben/upd/commit/4481b8bad6a02c6f761cf489a547b5546099e871))
- **audit**: order fix versions numerically, not lexicographically ([069eb1d](https://github.com/rvben/upd/commit/069eb1dc8771640376957339e40ada7b60a82b0a))

## [0.1.2](https://github.com/rvben/upd/compare/v0.1.1...v0.1.2) - 2026-04-24

### Added

- **cache**: add optional versions field to CacheEntry for future list_versions caching ([1beb34d](https://github.com/rvben/upd/commit/1beb34dc030f160e3748dff9a63e71bfa1772043))
- **output**: report held-back and skipped-by-cooldown packages ([3d1a2ce](https://github.com/rvben/upd/commit/3d1a2cef2ae31c59a87b417b074e0d672b7256d2))
- **updater**: propagate cooldown policy to remaining updaters ([8e80f25](https://github.com/rvben/upd/commit/8e80f252339f022d90f8120694e529c68c3bcf90))
- **updater**: apply cooldown policy in requirements updater ([5d6cfd3](https://github.com/rvben/upd/commit/5d6cfd32bfe87df7af86453476a93e2945f009ff))
- **registry**: implement list_versions for GitHub releases ([5f6472b](https://github.com/rvben/upd/commit/5f6472b5d7c4394d59fbabfd4bd9a1d9b736a67a))
- **registry**: implement list_versions for RubyGems ([1a1dda3](https://github.com/rvben/upd/commit/1a1dda31d73e0f25d8a73e6438aed7c4daadc007))
- **registry**: implement list_versions for Go module proxy ([196fef6](https://github.com/rvben/upd/commit/196fef634b0ae3c53096222e8bbe3161d8b67a33))
- **registry**: implement list_versions for crates.io ([8869dec](https://github.com/rvben/upd/commit/8869dec1a69148b5b3db44cc3393a05aaa2b01fb))
- **registry**: implement list_versions for npm ([b23cd78](https://github.com/rvben/upd/commit/b23cd787e748dfc5404a8fd51981c67f858e1d5a))
- **registry**: implement list_versions for PyPI ([9aa342c](https://github.com/rvben/upd/commit/9aa342c5c11ae504024aed5aa2d8930c66b4a6df))
- **cli**: add --min-age flag for cooldown override ([b5bfb30](https://github.com/rvben/upd/commit/b5bfb304c39f6b8099aef3a066e2ef4ed17f606f))
- **config**: show cooldown policy in --show-config ([9486257](https://github.com/rvben/upd/commit/9486257b22456e55e9af23709089a574cce262be))
- **config**: add [cooldown] table with default and per-ecosystem overrides ([a9ff8e3](https://github.com/rvben/upd/commit/a9ff8e31050485056a9bb6e03f4d313df1262998))
- **cooldown**: implement select() selection algorithm ([8b588bb](https://github.com/rvben/upd/commit/8b588bb1b42d84d696a7a17d8e97141a223351a1))
- **cooldown**: add CooldownPolicy with precedence resolution ([ddba284](https://github.com/rvben/upd/commit/ddba284329dad87bf84cf566ecb487ef665408a1))
- **cooldown**: add parse_duration for release-age config ([a7e67e0](https://github.com/rvben/upd/commit/a7e67e035764e4d905ace1dc4092f41c27510a5c))
- **registry**: re-export VersionMeta from crate root ([b2cdd60](https://github.com/rvben/upd/commit/b2cdd6037c09110881f53db4992542e77596f6c3))
- **registry**: add VersionMeta and list_versions trait method ([09ddbf9](https://github.com/rvben/upd/commit/09ddbf9c2b8f0546feafeb642f27547bed1882da))

### Fixed

- **cooldown**: harden selection against real-world constraints and per-file policy ([a284ea4](https://github.com/rvben/upd/commit/a284ea497dcf3abf65fda2c7e7f6c0c03c3dd8e2))
- **updater**: pass Poetry constraint to cooldown selection ([a0383e9](https://github.com/rvben/upd/commit/a0383e9167b2ef05cc4583aa23c1035d32268750))

## [0.1.1](https://github.com/rvben/upd/compare/v0.1.0...v0.1.1) - 2026-04-22

### Added

- **version**: add TagVersion for N-segment git tag parsing ([5994c6b](https://github.com/rvben/upd/commit/5994c6b39e347ed6470ca2097c1d7ed0a10b767d))

### Fixed

- **align**: use TagVersion fallback in compare_semver ([1738ace](https://github.com/rvben/upd/commit/1738aceaa98e39bd5245864e6bb1a2658c147878))
- **registry**: resolve N-segment git tags in GitHub fallback ([552425d](https://github.com/rvben/upd/commit/552425de91519cfb0d280eebc22c7802304d6580))

## [0.1.0](https://github.com/rvben/upd/compare/v0.0.28...v0.1.0) - 2026-04-21

### Breaking Changes

- **cli**: rename --bump to --only-bump and add --max-bump ([eb63589](https://github.com/rvben/upd/commit/eb63589867bac483b5de313d413d7c8e22a00a5f))
- **cli**: lock CLI surface for 0.1.0 ([d7a3ea4](https://github.com/rvben/upd/commit/d7a3ea441836e266c9ca3c3b772026246ba07d2f))

### Added

- **audit**: add SARIF 2.1.0 output for audit results ([d6b0118](https://github.com/rvben/upd/commit/d6b01188862bef90550814269df21c32f1588a50))
- **audit**: cache OSV responses and add --offline mode ([5a3058b](https://github.com/rvben/upd/commit/5a3058b39d97c4a116eefde65265bdfe354d263d))
- **audit**: add --fix-audit to bump packages to minimum safe version ([5292ae2](https://github.com/rvben/upd/commit/5292ae264b8f076c6b170f5eba5788e9d7eb56da))
- **cli**: rename --bump to --only-bump and add --max-bump ([eb63589](https://github.com/rvben/upd/commit/eb63589867bac483b5de313d413d7c8e22a00a5f))
- **cli**: scope no-args to VCS root and require --apply to mutate ([fe99418](https://github.com/rvben/upd/commit/fe99418b4844fa6c6944644e47982518a3f8616b))
- **audit**: normalize severity labels and sort by severity ([940f25c](https://github.com/rvben/upd/commit/940f25c0286deb5bb72d59cd08bec5ec6a34577e))
- **cli**: route errors to stderr and add --quiet flag ([0cbc19c](https://github.com/rvben/upd/commit/0cbc19c30f0c98a2683434c2f6b6f9f1cb9be615))
- **cli**: add --package filter to restrict updates by name ([f7962c8](https://github.com/rvben/upd/commit/f7962c8b1333a2da2133aacdc89f6f8318d0eb4e))
- **config**: warn on unknown keys and add --show-config ([cab49c1](https://github.com/rvben/upd/commit/cab49c18eb0ff1fd19f1e579959dc9ca3a555617))
- **lock**: regenerate packages.lock.json and .terraform.lock.hcl ([87d8e4e](https://github.com/rvben/upd/commit/87d8e4e9f7ea4e13ad0a5d4e4244384eae48b779))
- **audit**: include .NET packages via OSV NuGet ecosystem ([caec69d](https://github.com/rvben/upd/commit/caec69de65ae61f0923e19f1ba264031cc512365))
- **cli**: add --format json for machine-readable output ([f9c867f](https://github.com/rvben/upd/commit/f9c867fc497ed53e6d6997bb84660b40d851469a))

### Fixed

- **cli**: reject unknown subcommands instead of silent no-op ([e28aea4](https://github.com/rvben/upd/commit/e28aea44b783190f002a3453a1fc21ceff23c882))
- **terraform**: handle registry.terraform.io prefixed sources ([6d90d11](https://github.com/rvben/upd/commit/6d90d1175ab25b35d81dfff791329d5da8b34d8d))
- **cli**: print revert tip in --help and post-run summary ([05cdd14](https://github.com/rvben/upd/commit/05cdd14a5de31fc0a9533f6d6454bb5cb5b8c6d4))
- **lockfile**: error on missing tool, skip when no lockfile exists ([f8cca78](https://github.com/rvben/upd/commit/f8cca785f8a365ee7240cc60236b92387253afdb))
- **cli**: accept comma-separated values for --lang ([c7f8b11](https://github.com/rvben/upd/commit/c7f8b11564b872270747f1cb88b2dbb988060bf3))
- **main**: exit 1 on --dry-run with pending updates ([eb3cadc](https://github.com/rvben/upd/commit/eb3cadc79f03f33f5a9ce5cc26ecec74c804b103))
- **audit**: exit 3 on vulnerabilities, add --no-fail ([28e8b75](https://github.com/rvben/upd/commit/28e8b75ad7b9ff15f33dfd56c5a8270e3dc1696b))
- **main**: exit 2 on errors, structure JSON error objects ([353e013](https://github.com/rvben/upd/commit/353e013988cb43bd66544246e1dca0a5132d4263))
- **version**: keep pre-releases on pre-release-pinned packages ([a95d2f8](https://github.com/rvben/upd/commit/a95d2f85c4143cc913266df774bda3fe35a0a4d3))
- **terraform**: keep ~> constraint when latest still satisfies ([e869e40](https://github.com/rvben/upd/commit/e869e40f99ca88cda873556cfdaff06c44b8de53))
- **audit**: include Go pseudoversion dependencies ([e051f06](https://github.com/rvben/upd/commit/e051f0621a88751059f83707bc415df359b15905))
- **interactive**: require TTY for --interactive mode ([ba0d0b2](https://github.com/rvben/upd/commit/ba0d0b2e2bb7d021ea557bf547bade3be5953379))
- **updater**: refuse to write version downgrades ([41bd7e6](https://github.com/rvben/upd/commit/41bd7e67d03d48cb2f948770abc7ee4979205f9e))
- **requirements**: skip update when current is not valid PEP 440 ([4e6f3ea](https://github.com/rvben/upd/commit/4e6f3ea755d974392915e3fe211b6e0f9e6c3121))
- **audit**: preserve package-name case for OSV queries ([8bde8b1](https://github.com/rvben/upd/commit/8bde8b1bc81aba56a43049d5fac46016195d7eac))
- **rubygems**: skip yanked versions when selecting latest ([2d48a0e](https://github.com/rvben/upd/commit/2d48a0ebcce2c576ca0169f661f27bd4a268a18c))

## [0.0.28](https://github.com/rvben/upd/compare/v0.0.27...v0.0.28) - 2026-04-17

### Added

- **updater**: recursive hidden-file discovery, precise line numbers, scoped npm ([5fcc5d8](https://github.com/rvben/upd/commit/5fcc5d818d349abd109ae7cac001972a6a9cadea))

### Fixed

- **package_json**: index dependencies when opening brace starts on its own line ([e40c3f1](https://github.com/rvben/upd/commit/e40c3f1bf736ef3ea0c565047d886ff7543d37c9))
- **update**: check mode exits 1 when only configured pins differ ([33a69f5](https://github.com/rvben/upd/commit/33a69f5a16ee03247a13c41bfabe1935d09bfa64))
- **updater**: classify configured pins as pins, not updates ([571a96b](https://github.com/rvben/upd/commit/571a96b9de72fe283c5114e594da72687a67efab))

## [0.0.27](https://github.com/rvben/upd/compare/v0.0.26...v0.0.27) - 2026-04-15

### Fixed

- **align**: use pep440_rs for Python stable-version check ([7f132b3](https://github.com/rvben/upd/commit/7f132b351cdd9225a31df96d2a421c8c42926987))
- **version**: use PEP 440 release segments for precision matching ([fff041d](https://github.com/rvben/upd/commit/fff041d2d2e9508f117012a6bfc857ee57e5cd20))

## [0.0.26](https://github.com/rvben/upd/compare/v0.0.25...v0.0.26) - 2026-04-15

### Fixed

- **pypi**: rewrite HTML Simple API parser to handle multi-line anchor tags ([f9c937b](https://github.com/rvben/upd/commit/f9c937be297112e0556cae205f8c0f3ce54997f4))

## [0.0.25](https://github.com/rvben/upd/compare/v0.0.24...v0.0.25) - 2026-04-15

### Fixed

- **pypi**: handle string-valued yanked field in PEP 691 JSON Simple API ([b17034b](https://github.com/rvben/upd/commit/b17034b540f6a5b62e446131c6e87f43695bbd9b))

## [0.0.24] - 2026-03-23

### Added

- **NuGet/.NET support**: Update `PackageReference` and `PackageVersion` elements in `.csproj` and `Directory.Packages.props` files via the NuGet v3 API
- **Gemfile.lock regeneration**: `--lock` flag now supports Ruby projects (runs `bundle install`)

## [0.0.23] - 2026-03-23

### Added

- **Pre-commit support**: Update hook versions in `.pre-commit-config.yaml` via GitHub releases
- **Ruby Gemfile support**: Update gem versions with RubyGems registry and pessimistic constraint (`~>`) support
- **Mise/asdf support**: Update tool versions in `.mise.toml` and `.tool-versions` for 24+ mapped dev tools
- **Terraform/OpenTofu support**: Update provider and module versions in `.tf` files via the Terraform Registry API

### Fixed

- All updaters now use safe HashMap lookups (no panics on edge cases)
- Version replacement no longer clobbers inline comments
- Duplicate registry lookups deduplicated across all updaters

## [0.0.22] - 2026-03-23

### Added

- **Pre-commit support**: Update hook versions in `.pre-commit-config.yaml`
  - Reuses GitHub releases API for version lookups
  - Skips local, meta, and non-GitHub repos
  - Filter with `--lang pre-commit`
- **Ruby Gemfile support**: Update gem versions in `Gemfile`
  - New RubyGems registry with pessimistic constraint (`~>`) support
  - Preserves version operators (`~>`, `>=`, exact)
  - Filter with `--lang ruby`
- **Mise/asdf support**: Update tool versions in `.mise.toml` and `.tool-versions`
  - Maps 24+ common dev tools to GitHub releases (node, python, go, rust, zig, deno, bun, uv, ruff, etc.)
  - Skips `latest` and `cargo:*` entries
  - Filter with `--lang mise`

### Fixed

- All updaters now use safe HashMap lookups (no panics on edge cases)
- Version replacement no longer clobbers inline comments
- Duplicate registry lookups deduplicated across all updaters

## [0.0.21] - 2026-03-23

### Added

- **GitHub Actions support**: Update action version references in `.github/workflows/*.yml` files
  - Preserves version precision (`@v4` stays major-only, `@v4.1.0` stays exact)
  - Skips SHA-pinned actions, branch refs, local actions, and Docker references
  - Authentication via `GITHUB_TOKEN` or `GH_TOKEN` for higher rate limits
  - Filter with `--lang actions`, works with all existing flags

### Fixed

- Rate limit and access denied errors now include hints about setting authentication tokens
- Fixed potential panic in align command for path-based file types

## [0.0.20] - 2025-12-19

### Fixed

- **PyPI registry URL format**: Corrected default PyPI base URL from `https://pypi.org/pypi` to `https://pypi.org`
  - Fixed Simple API URL construction: now correctly uses `https://pypi.org/simple/{package}/` instead of malformed `https://pypi.org/pypi/simple/{package}/`
  - Fixed "Package exists but has no suitable versions" errors for valid packages
  - Resolves CloudFlare challenge page responses that prevented package lookups
  - Particularly affects packages in PEP 735 dependency-groups sections

## [0.0.19] - 2025-12-19

### Fixed

- **Improved error messages for common failures**:
  - HTTP client creation failures now explain TLS/SSL configuration issues
  - HTTP errors categorized by status code (401, 403, 404, 429, 5xx)
  - Registry-specific credential hints for authentication errors (PyPI, npm, crates.io, Go)
  - TOML parsing errors now include file path and line numbers
  - "Package not found" (404) distinguished from "no versions available" (yanked/pre-release only)
  - Config file errors (`--config` flag) now show detailed messages instead of silent failure

## [0.0.18] - 2025-12-18

### Added

- **Configuration file support** (`.updrc.toml`, `upd.toml`, `.updrc`)
  - `ignore`: List of packages to skip during updates
  - `pin`: Map of packages to pinned versions (bypasses registry lookup)
  - Config file discovery walks up directory tree to find project root config
  - Use `--config` flag to specify a custom config file path
- **Enhanced private registry authentication**:
  - PyPI: Read `pip.conf` / `pip.ini` for `index-url` and `extra-index-url`
  - npm: Support for scoped registries in `.npmrc` (`@scope:registry=...`)
  - Cargo: Read `~/.cargo/config.toml` for custom registry URLs
  - Go: Support `GOPRIVATE`, `GONOPROXY`, `GONOSUMDB` environment variables

### Changed

- Verbose output now shows ignored and pinned packages
- Summary output shows counts of pinned and ignored packages

## [0.0.17] - 2025-12-17

### Added

- Pre-commit hook support via `.pre-commit-hooks.yaml`
  - `upd-check`: Fail if any dependencies are outdated
  - `upd-check-major`: Fail only on major (breaking) updates
- Lockfile regeneration with `--lock` flag:
  - `Cargo.lock` via `cargo generate-lockfile`
  - `go.sum` via `go mod tidy`
  - `bun.lockb` via `bun install`
  - `package-lock.json` via `npm install`
  - `poetry.lock` via `poetry lock`

## [0.0.16] - 2025-12-17

### Added

- Parallel file processing for faster updates across multiple files
- `--lock` flag to regenerate lockfiles after updating dependencies

### Fixed

- CLI description now mentions Rust and Go ecosystems

## [0.0.15] - 2025-12-17

### Added

- Private registry compatibility improvements for enterprise PyPI servers
- Better handling of non-standard Simple API responses

## [0.0.14] - 2025-12-17

### Fixed

- Skip yanked packages when fetching versions from Simple API responses
- Prevents updates to withdrawn/yanked package versions

## [0.0.13] - 2025-12-17

### Added

- Simple API fallback for private PyPI servers that don't support JSON API
- Automatic detection and parsing of HTML Simple API responses

## [0.0.12] - 2025-12-17

### Fixed

- Normalize Simple API URLs to JSON API format for consistent handling
- Better URL handling for various private PyPI server configurations

## [0.0.11] - 2025-12-17

### Added

- `UV_EXTRA_INDEX_URL` and `PIP_EXTRA_INDEX_URL` environment variable support
- Query multiple package indexes when primary index doesn't have the package

## [0.0.10] - 2025-12-17

### Added

- `audit` subcommand for security vulnerability scanning via OSV (Open Source Vulnerabilities) API
  - Scans all dependency files for known vulnerabilities
  - Supports all ecosystems: PyPI, npm, crates.io, Go
  - Shows CVSS severity scores, descriptions, and fixed versions
  - Batch queries for efficiency (up to 1000 packages per request)
  - Parallel fetching of vulnerability details for performance
  - Use `--check` flag for CI integration (exit 1 if vulnerabilities found)
  - Use `--lang` to filter by ecosystem

## [0.0.9] - 2025-12-11

### Added

- Private repository authentication support for all registries:
  - PyPI: Basic Auth via environment variables, `~/.netrc`, or inline URL credentials
  - npm: Bearer token via `NPM_TOKEN`, `NODE_AUTH_TOKEN`, or `.npmrc`
  - Cargo: Token via `CARGO_REGISTRY_TOKEN` or `~/.cargo/credentials.toml`
  - Go: Basic Auth via `GOPROXY_USERNAME`/`GOPROXY_PASSWORD` or `~/.netrc`
- Inline index URL support in `requirements.txt` (`--index-url`, `-i`)

### Fixed

- Upper-bound-only constraints like `django<6` are now skipped (not incorrectly narrowed)
- Constraints with upper bounds now preserve the upper bound during updates
  - `django>=4.0,<6` → `django>=5.2,<6` (previously dropped the `<6`)

## [0.0.8] - 2025-12-10

### Added

- `--check` / `-c` flag for CI integration (exit code 1 if updates available)
- Interactive mode (`-i`) for approving updates one by one

## [0.0.7] - 2025-12-10

### Added

- `align` subcommand to align package versions across multiple files in a monorepo

## [0.0.6] - 2025-12-09

### Added

- `--lang` / `-l` flag to filter updates by language/ecosystem
- Update type filters: `--major`, `--minor`, `--patch`

## [0.0.5] - 2025-12-09

### Added

- `--full-precision` flag to output full version numbers instead of matching original precision
- Clickable `file:line:` output format for update messages (recognized by VS Code, iTerm2, and modern terminals)
- Support for Rust `Cargo.toml` dependencies
- Support for Go `go.mod` dependencies
- HTTP retry logic with exponential backoff for transient network errors

### Changed

- Version precision now preserved by default (e.g., `flask>=2.0` stays `2.x` format, not `2.x.y`)
- Removed unused `--verify` flag from CLI

### Fixed

- Output now includes line numbers for each updated dependency

### Testing

- Comprehensive test coverage for all updaters (requirements.txt, pyproject.toml, package.json, Cargo.toml, go.mod)
- Integration tests with MockRegistry for offline testing
- Tests for HTTP retry logic, CLI argument parsing, version classification

## [0.0.4] - 2025-12-08

### Fixed

- Use rustls-tls instead of native-tls for better cross-compilation support
- Update to Rust 1.91.1

## [0.0.3] - 2025-12-08

### Fixed

- CI workflow improvements for cross-platform builds

## [0.0.2] - 2025-12-08

### Added

- Support for Poetry-style `[tool.poetry.dependencies]` in pyproject.toml
- Pre-release version handling (packages with alpha/beta versions update to newer pre-releases)

## [0.0.1] - 2025-12-08

### Added

- Initial release of `upd` - a fast dependency updater written in Rust
- Support for Python dependency files:
  - `requirements.txt` and `requirements-*.txt` patterns
  - `requirements.in` and `requirements-*.in` patterns
  - `pyproject.toml` with `[project.dependencies]` and `[project.optional-dependencies]`
- Support for Node.js dependency files:
  - `package.json` with `dependencies` and `devDependencies`
- Version constraint handling:
  - Respects upper bounds (e.g., `>=2.0,<3` won't update to v3.x)
  - PEP 440 version specifier support for Python
  - Semver range support for npm packages
- Major version bump warnings with `(MAJOR)` indicator
- Pre-release version filtering (excludes alpha, beta, rc versions)
- Dry-run mode (`-n`) to preview changes without modifying files
- Format-preserving updates using `toml_edit` for pyproject.toml
- Gitignore-aware file discovery (respects `.gitignore` patterns)
- Version caching for faster subsequent runs
- Colored terminal output with `--no-color` option
- Self-update command (`upd self-update`)
- Cache management (`upd clean-cache`)

### Performance

- Async HTTP requests with `reqwest`
- Concurrent dependency lookups
- Release binary with LTO optimization
