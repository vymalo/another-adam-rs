# Changelog

## [0.8.0](https://github.com/vymalo/another-adam-rs/compare/v0.7.0...v0.8.0) (2026-11-28)


### ⚠ BREAKING CHANGES

* **deploy:** a values file that sets cache.create or cache.existingClaim renders no cache (the chart has no values.schema.json, so Helm ignores the keys). The chart no longer creates a PersistentVolumeClaim.
* **plugin:** Decision replaces Verdict, and a decision may ask for an action ([#560](https://github.com/vymalo/another-adam-rs/issues/560))

### Features

* **adam-runtime:** a run records the journal epoch it started on (ANR-961, ADR 0076 slices 1-2) ([#593](https://github.com/vymalo/another-adam-rs/issues/593)) ([5684908](https://github.com/vymalo/another-adam-rs/commit/56849085b966d1f00b839f4232d828b02f2a9d97))
* **adam-runtime:** a worker keeps a run it refused for its epoch out of its claims (ANR-963, ADR 0076 slice 3) ([#595](https://github.com/vymalo/another-adam-rs/issues/595)) ([8f071e2](https://github.com/vymalo/another-adam-rs/commit/8f071e25a9de70d9f04adcc9b5caf17c7924f6ef))
* **agent:** MEMORY_URL selects the L2 store and its adapter (ANR-1459) ([#589](https://github.com/vymalo/another-adam-rs/issues/589)) ([8cb80fe](https://github.com/vymalo/another-adam-rs/commit/8cb80fe4f7e5eb1171bda4632cf831b1576e539d))
* **assembly:** every tool source declares its tools (ANR-1319) ([#532](https://github.com/vymalo/another-adam-rs/issues/532)) ([b7715dc](https://github.com/vymalo/another-adam-rs/commit/b7715dcb8a7b5362a39de23aa8f0773e5059889c))
* **ci:** keep every workflow pin on its recorded major (ANR-1427) ([#548](https://github.com/vymalo/another-adam-rs/issues/548)) ([ecfa536](https://github.com/vymalo/another-adam-rs/commit/ecfa53616fcdf1e162078636691eea9a62f4a99e))
* **coder:** run a call's independent checks at the same time (ANR-243) ([#526](https://github.com/vymalo/another-adam-rs/issues/526)) ([1add6ed](https://github.com/vymalo/another-adam-rs/commit/1add6ede494895a19c7f173fcaf10fe69d01b1da))
* **deploy:** no shared cache; run pods admit cache.allowedClaims ([#604](https://github.com/vymalo/another-adam-rs/issues/604)) ([024ba0f](https://github.com/vymalo/another-adam-rs/commit/024ba0f580a67e1a4e207c0fb3b64bf6735a1aac))
* **dev:** a ready-to-run usage/v1 and reasoning check for the ANR-75 human test ([#585](https://github.com/vymalo/another-adam-rs/issues/585)) ([8947d34](https://github.com/vymalo/another-adam-rs/commit/8947d34762de6cc3c81b7b7634e8a1ccdfbeb008))
* **engine:** a local agent over the person's directory, asking before a write (ANR-236) ([#547](https://github.com/vymalo/another-adam-rs/issues/547)) ([23cda5e](https://github.com/vymalo/another-adam-rs/commit/23cda5e23c9f3d68416751bc34a791b29ef30e9a))
* **engine:** a Say carries the access tier its message works under (ANR-77) ([#556](https://github.com/vymalo/another-adam-rs/issues/556)) ([c524257](https://github.com/vymalo/another-adam-rs/commit/c524257f00c69b8e8076e7a12cab83b172fa542c))
* **hooks:** the deployment's hook programs are wired into both binaries (ANR-1301) ([#605](https://github.com/vymalo/another-adam-rs/issues/605)) ([90a0de2](https://github.com/vymalo/another-adam-rs/commit/90a0de20102e4590024c6f907bb0b5c3992f8e91))
* **memory:** the Redis adapter for the L2 memory port (ANR-1294) ([#537](https://github.com/vymalo/another-adam-rs/issues/537)) ([4a35d7a](https://github.com/vymalo/another-adam-rs/commit/4a35d7a27b15b48a671167c3a8289f5c9634f73b))
* **model-openai:** the thinking_budget wire keeps a thinking object's siblings (ANR-55) ([#544](https://github.com/vymalo/another-adam-rs/issues/544)) ([5696f59](https://github.com/vymalo/another-adam-rs/commit/5696f599ba8f54ad287a69ebb0691889a072cb9c))
* **mongodb:** the L2 Memory port over MongoDB, behind a `memory` feature (ANR-1293) ([#539](https://github.com/vymalo/another-adam-rs/issues/539)) ([00a714c](https://github.com/vymalo/another-adam-rs/commit/00a714c4d4e4515638b2d7e5420a553c46654557))
* **otel:** a gauge of the store's busy count ([#601](https://github.com/vymalo/another-adam-rs/issues/601)) ([2a6300b](https://github.com/vymalo/another-adam-rs/commit/2a6300b1b651826000df8f5c93d07d6a963d9c79))
* **otel:** a model call's tokens and latency, by direction and alias (ANR-1462) ([#598](https://github.com/vymalo/another-adam-rs/issues/598)) ([35463fb](https://github.com/vymalo/another-adam-rs/commit/35463fb7258085064b08982aa26a65a0c756d9f2))
* **otel:** logs over OTLP through an allowlist (ANR-1348) ([#566](https://github.com/vymalo/another-adam-rs/issues/566)) ([6545fd0](https://github.com/vymalo/another-adam-rs/commit/6545fd0a1f841ac3057bf0e8485a9f20e943bb32))
* **otel:** runs by state, the first instrument family (ANR-1449) ([#563](https://github.com/vymalo/another-adam-rs/issues/563)) ([b059919](https://github.com/vymalo/another-adam-rs/commit/b059919fa1ee6732342b93558210a05bfb9e0075))
* **otel:** the four binaries compose the OTel layer from one init (ANR-1412) ([#557](https://github.com/vymalo/another-adam-rs/issues/557)) ([a3a6207](https://github.com/vymalo/another-adam-rs/commit/a3a6207b4faf5cfdd62fe9cb4d6d4eb76a89821a))
* **otel:** the metrics half on the same switch, with closed label sets (ANR-1448) ([#559](https://github.com/vymalo/another-adam-rs/issues/559)) ([8d9d1f9](https://github.com/vymalo/another-adam-rs/commit/8d9d1f98fd1a80e81d45327f2b0058a1ddde03de))
* **otel:** the OTLP/HTTP foundation, off unless an endpoint is set (ANR-1350) ([#538](https://github.com/vymalo/another-adam-rs/issues/538)) ([2d87819](https://github.com/vymalo/another-adam-rs/commit/2d8781955d3ffd5ff1581e98ac659a7e528db62d))
* **otel:** the run dashboards, generated by a script and held to the emitted list (ANR-1349) ([#570](https://github.com/vymalo/another-adam-rs/issues/570)) ([f952aaa](https://github.com/vymalo/another-adam-rs/commit/f952aaa4a5bcd45ea17eb7d46c0c6ffc76f95245))
* **otel:** tool latency by name, the second instrument family (ANR-1454) ([#575](https://github.com/vymalo/another-adam-rs/issues/575)) ([438bce4](https://github.com/vymalo/another-adam-rs/commit/438bce47d710331a9e8a11bdfda2e58dc75729ab))
* **plugin:** Decision replaces Verdict, and a decision may ask for an action ([#560](https://github.com/vymalo/another-adam-rs/issues/560)) ([d658157](https://github.com/vymalo/another-adam-rs/commit/d6581574f92f558b9e6d855b506805ead1a58de7))
* **run-cert:** the broker adapter of RunCertIssuer and the RUN_CERT_* configuration (ANR-1310) ([#533](https://github.com/vymalo/another-adam-rs/issues/533)) ([06d6e7c](https://github.com/vymalo/another-adam-rs/commit/06d6e7c56ec0571b315334a84f0a24d3814106a5))
* **run-signer:** a loopback sidecar that signs a run's model requests (ANR-1311) ([#564](https://github.com/vymalo/another-adam-rs/issues/564)) ([0ea492d](https://github.com/vymalo/another-adam-rs/commit/0ea492d1392ff2956bda7155d86699a51670a1b1))
* **runtime:** a declared shape admits a relayed server's tools (ANR-1390) ([#542](https://github.com/vymalo/another-adam-rs/issues/542)) ([31f9a16](https://github.com/vymalo/another-adam-rs/commit/31f9a166078af5a26899178961b1ef5678926e1d))
* **sessions:** a session names a model alias from the set it may use (ANR-1487) ([#596](https://github.com/vymalo/another-adam-rs/issues/596)) ([8118970](https://github.com/vymalo/another-adam-rs/commit/8118970d23f721c9ada178a111816956dc1bf148))
* **sessions:** subagent_edit, a session's config changed within its bounds (ANR-216) ([#546](https://github.com/vymalo/another-adam-rs/issues/546)) ([7d9446f](https://github.com/vymalo/another-adam-rs/commit/7d9446f3c19a2533a6b56b1fbaa7c6396746bcd1))
* **si-memory:** the handle names its caller per call, and bind offers the tools (WIP) ([#549](https://github.com/vymalo/another-adam-rs/issues/549)) ([4db3dc3](https://github.com/vymalo/another-adam-rs/commit/4db3dc3e28d628724b42ebbedce12a8f3e6e5eeb))
* **store-postgres:** the PostgreSQL adapter for the L2 memory port ([#562](https://github.com/vymalo/another-adam-rs/issues/562)) ([6596b12](https://github.com/vymalo/another-adam-rs/commit/6596b124d2f23dbc7db353fd2b39e6cf25c7262d))
* training export, model context windows, child sessions, bounded tool output, six language servers (ADRs 0087-0091) ([#487](https://github.com/vymalo/another-adam-rs/issues/487)) ([ff3babf](https://github.com/vymalo/another-adam-rs/commit/ff3babf84f9b9b806b3ecc37c540ff3827d1b8f8))
* **workspace:** a command environment is a list of hooks (ANR-160) ([#543](https://github.com/vymalo/another-adam-rs/issues/543)) ([ba92efa](https://github.com/vymalo/another-adam-rs/commit/ba92efa7f71fa660eedb332836a6bd52d6a77b22))


### Bug Fixes

* **build:** the two small servers declare adam-otel directly, so the workspace parses ([#565](https://github.com/vymalo/another-adam-rs/issues/565)) ([d039821](https://github.com/vymalo/another-adam-rs/commit/d039821dde0ac45e382103929a3f6dd901017234))
* **ci:** a push to main gets its own concurrency group per commit (ANR-1360) ([#550](https://github.com/vymalo/another-adam-rs/issues/550)) ([2356e95](https://github.com/vymalo/another-adam-rs/commit/2356e953350f5a2f40e12025a9a4ca7beaa475c3))
* **ci:** release-please sees every merge, and docs-site says why it cancels (ANR-1457) ([#574](https://github.com/vymalo/another-adam-rs/issues/574)) ([9abdaa7](https://github.com/vymalo/another-adam-rs/commit/9abdaa770ba775c720155cdafef49357285f6ba7))
* **ci:** the release-tools job runs on node24 actions again (ANR-1410) ([#545](https://github.com/vymalo/another-adam-rs/issues/545)) ([2fe3b13](https://github.com/vymalo/another-adam-rs/commit/2fe3b131369a4a3eb86bc6e8dd0b7c0876fae5f6))
* **ci:** the required lint check counts a red tls-provider (ANR-1440) ([#553](https://github.com/vymalo/another-adam-rs/issues/553)) ([8517408](https://github.com/vymalo/another-adam-rs/commit/8517408dbb8e9b948b7d7713774ea9d8a28f26a3))
* **ci:** three release.yml pin comments name the tag their SHA really is (ANR-1428) ([#555](https://github.com/vymalo/another-adam-rs/issues/555)) ([41afcce](https://github.com/vymalo/another-adam-rs/commit/41afccee178ed1d6adfa0cf325123ebc1d684f91))
* **coder:** a check's pass is reused only at its own epoch (ANR-247) ([#581](https://github.com/vymalo/another-adam-rs/issues/581)) ([46f86ab](https://github.com/vymalo/another-adam-rs/commit/46f86ab9acb4f7752867754137e54854f7b9fa10))
* **coder:** the language-server readiness tests gate on the latch, not the clock ([#592](https://github.com/vymalo/another-adam-rs/issues/592)) ([e130e97](https://github.com/vymalo/another-adam-rs/commit/e130e97a28ca9a2f3b75e41aa2c52512a3c7cb7e))
* **coder:** the turn-key names stay within 63 characters ([#541](https://github.com/vymalo/another-adam-rs/issues/541)) ([7a9d059](https://github.com/vymalo/another-adam-rs/commit/7a9d059f97436a346c1a4eeddccf8a4120763479))
* **engine:** the access-tier test stops shadowing its own helpers ([#568](https://github.com/vymalo/another-adam-rs/issues/568)) ([3dc7d68](https://github.com/vymalo/another-adam-rs/commit/3dc7d6853bbcee95994d2ed49f4ce416f64b0ef9))
* **engine:** the local agent's hook passes clippy -D warnings ([#584](https://github.com/vymalo/another-adam-rs/issues/584)) ([9aef807](https://github.com/vymalo/another-adam-rs/commit/9aef807db640ebc411fbc559e9b665899ae81a28))
* **hooks:** a queued event ends its line, and the tests count requests not lines (ANR-1453) ([#576](https://github.com/vymalo/another-adam-rs/issues/576)) ([d7f2be2](https://github.com/vymalo/another-adam-rs/commit/d7f2be2d78cfbe5d408ff96b7190f9f5431e90d8))
* **hooks:** the environment test asserts the whole set, and its import is used ([#535](https://github.com/vymalo/another-adam-rs/issues/535)) ([78e6689](https://github.com/vymalo/another-adam-rs/commit/78e668973396cb68dc3c4ecb76e6487d63d062f7))
* **llm-agent:** build the test's SessionConfig field by field ([#558](https://github.com/vymalo/another-adam-rs/issues/558)) ([9b20717](https://github.com/vymalo/another-adam-rs/commit/9b2071772c97deb997f15c6fa3d8a91a796376af))
* **llm-agent:** keep a group's loader with the tools of its server (ANR-1304) ([#536](https://github.com/vymalo/another-adam-rs/issues/536)) ([b4a38b2](https://github.com/vymalo/another-adam-rs/commit/b4a38b276db9fa5a95e48edf1e83e86174b9bca0))
* **llm-agent:** the call-span tests build the span inside the subscriber's scope ([#586](https://github.com/vymalo/another-adam-rs/issues/586)) ([fe2567e](https://github.com/vymalo/another-adam-rs/commit/fe2567ee74657f90dc504c07ea906f9445de1df5))
* **llm-agent:** the three remote-subagent cases serialise on the declared name they share ([#599](https://github.com/vymalo/another-adam-rs/issues/599)) ([56cff83](https://github.com/vymalo/another-adam-rs/commit/56cff8370c2e4b1213fceb7af0aa557cf5db86c3))
* **logs:** the eight interpolated messages log a static message and a field (ANR-1456) ([#569](https://github.com/vymalo/another-adam-rs/issues/569)) ([e5e0ce4](https://github.com/vymalo/another-adam-rs/commit/e5e0ce4341a13fd537dd586cac163f19c59d360a))
* **logs:** the fifteen messages a line scan cannot see, and a check that reads them (ANR-1458) ([#572](https://github.com/vymalo/another-adam-rs/issues/572)) ([2ff12e8](https://github.com/vymalo/another-adam-rs/commit/2ff12e8633e8f7fe10b5571fe6d821dd2eafffb8))
* **make:** the guard settles before after_run reads its report (ANR-1455) ([#579](https://github.com/vymalo/another-adam-rs/issues/579)) ([ccab10e](https://github.com/vymalo/another-adam-rs/commit/ccab10e2e0b90e994d96f40ff419d34daece0c57))
* **memory:** qualify the Memory::record link in the lesson rules ([#578](https://github.com/vymalo/another-adam-rs/issues/578)) ([ce478de](https://github.com/vymalo/another-adam-rs/commit/ce478de91251d352446372f45caef55ac05e0b3c))
* **mongodb:** recall and retire refuse an empty configured scope (ANR-1426) ([#552](https://github.com/vymalo/another-adam-rs/issues/552)) ([8ae65e6](https://github.com/vymalo/another-adam-rs/commit/8ae65e6e803afec992a4e434c892ff0853b51595))
* **otel:** allow the manual_contains shape the labels' lifetimes need ([#591](https://github.com/vymalo/another-adam-rs/issues/591)) ([d3d35bf](https://github.com/vymalo/another-adam-rs/commit/d3d35bf23a00bda8acf2d70a9f7eca4f89d953e2))
* **otel:** drop the unused logs import that makes clippy red on main ([#573](https://github.com/vymalo/another-adam-rs/issues/573)) ([e6ecb54](https://github.com/vymalo/another-adam-rs/commit/e6ecb54e60e4ca1ec71a0bab1514cdf88a75a110))
* **otel:** only the binaries that export link a TLS provider (ANR-1480) ([#587](https://github.com/vymalo/another-adam-rs/issues/587)) ([7594fbb](https://github.com/vymalo/another-adam-rs/commit/7594fbb6cdc1306e232fd739615afc94d87a5d0a))
* **sessions:** a deleted working session is gone when its round stops (ANR-1229) ([#590](https://github.com/vymalo/another-adam-rs/issues/590)) ([ee7d81d](https://github.com/vymalo/another-adam-rs/commit/ee7d81d7008873e70e9be9e301416ab25e758330))
* **store-postgres:** link the blocking bridge to the module page ([#577](https://github.com/vymalo/another-adam-rs/issues/577)) ([81177f2](https://github.com/vymalo/another-adam-rs/commit/81177f29b5f9e34f8dba935abab1d8336afdde0a))
* **tools:** the sqlite-probe lock is regenerated, so --locked resolves it ([#583](https://github.com/vymalo/another-adam-rs/issues/583)) ([72ab4d6](https://github.com/vymalo/another-adam-rs/commit/72ab4d6e6cb8d73d6dd5b6f1391eed45ebef0dfa))


### Documentation

* **env:** SI_MEMORY is read by adam-agent only, for now ([#540](https://github.com/vymalo/another-adam-rs/issues/540)) ([8932c84](https://github.com/vymalo/another-adam-rs/commit/8932c84b97d976dddcab9c4361de9aedeb2eb5c9))
* **otel:** main's rustdoc builds again — two links that cannot resolve and a table row rustdoc refuses ([#571](https://github.com/vymalo/another-adam-rs/issues/571)) ([c0be716](https://github.com/vymalo/another-adam-rs/commit/c0be71616e371b04ace57064dff59408c85a41ba))
* **roadmap:** a roadmap of what is missing, incomplete and long ([#534](https://github.com/vymalo/another-adam-rs/issues/534)) ([0513e94](https://github.com/vymalo/another-adam-rs/commit/0513e9410c511941ce12f4972abd47f594c181b5))
* the runtime README's worker section, and a dated note under slice 3 in ADR 0076. ([8f071e2](https://github.com/vymalo/another-adam-rs/commit/8f071e25a9de70d9f04adcc9b5caf17c7924f6ef))


### Tests

* **a2a:** the usage counts serialise as whole numbers (ANR-1470) ([#602](https://github.com/vymalo/another-adam-rs/issues/602)) ([dfa045e](https://github.com/vymalo/another-adam-rs/commit/dfa045ef29d4c61c6b6e1da46806861ba02ec6cc))
* **agent:** a stream error waits for a started call, and a replay keeps the order at a lower limit ([#594](https://github.com/vymalo/another-adam-rs/issues/594)) ([ec9a815](https://github.com/vymalo/another-adam-rs/commit/ec9a8157298be79d256d546c377be3fe6fe3b866))
* **coder:** a measured resume budget from process start to the first claim (ANR-759) ([#551](https://github.com/vymalo/another-adam-rs/issues/551)) ([9673ee7](https://github.com/vymalo/another-adam-rs/commit/9673ee74abeca0511501744b2af877aeb43c29fb))
* **coder:** recalibrate the compaction budget to the prompt it now is ([#580](https://github.com/vymalo/another-adam-rs/issues/580)) ([21b7e6e](https://github.com/vymalo/another-adam-rs/commit/21b7e6ecbcf868f1395e9e193803bd0e50be253b))
* **coder:** start run-signer on loopback in the image smoke (ANR-1313) ([#567](https://github.com/vymalo/another-adam-rs/issues/567)) ([b84e268](https://github.com/vymalo/another-adam-rs/commit/b84e268bb91099e090591b607cf46dd17c772bae))
* **dev:** an uploads/v1 e2e — the file goes to the bucket and the artifact names it (ANR-1208) ([8d80b25](https://github.com/vymalo/another-adam-rs/commit/8d80b2552bc677c9d6d2599c30c89ca379224210))
* **dev:** an uploads/v1 e2e — the file goes to the bucket and the artifact names it (ANR-1208) ([#606](https://github.com/vymalo/another-adam-rs/issues/606)) ([8d80b25](https://github.com/vymalo/another-adam-rs/commit/8d80b2552bc677c9d6d2599c30c89ca379224210))
* **hooks:** script the same tool call for every retry, so the run fails as the contract says (ANR-1466) ([#582](https://github.com/vymalo/another-adam-rs/issues/582)) ([e12a31e](https://github.com/vymalo/another-adam-rs/commit/e12a31e6ae9b93767388a4604fbcdc3436dadac5))
* **memory:** the kit proves an empty configured scope is refused by every call (ANR-1415) ([#554](https://github.com/vymalo/another-adam-rs/issues/554)) ([07fba5c](https://github.com/vymalo/another-adam-rs/commit/07fba5caed2922b28b34cdbc6b3ba7624e84e908))
* **model:** the run-key test asks the fake issuer with a real CSR ([#561](https://github.com/vymalo/another-adam-rs/issues/561)) ([f68e640](https://github.com/vymalo/another-adam-rs/commit/f68e640d5d10f01fcc9b0c4fed57e144d4226d63))
* **sessions:** the retried edit's script repeats its call, as the retry re-asks the model ([#597](https://github.com/vymalo/another-adam-rs/issues/597)) ([1c7188b](https://github.com/vymalo/another-adam-rs/commit/1c7188b0d0dfb1ab7617c9d8f2ba3880507b543a))


### Chores

* **deny:** allow BSL-1.0, the redis crate's unconditional dependency (ANR-1483) ([#588](https://github.com/vymalo/another-adam-rs/issues/588)) ([983c7f2](https://github.com/vymalo/another-adam-rs/commit/983c7f2272df8ca4c5e808642aac97f1f26a86ad))
* **deploy:** bump coder to sha-3dc7d68 ([51a8c03](https://github.com/vymalo/another-adam-rs/commit/51a8c03e782496ab8f49482a06c9505fafc42036))
* **deploy:** bump coder to sha-430718b ([cd4bc00](https://github.com/vymalo/another-adam-rs/commit/cd4bc00de1060a8b7a46360731073824438cdd85))
* **deploy:** bump coder to sha-c524257 ([43a3227](https://github.com/vymalo/another-adam-rs/commit/43a3227757448468988e965c8b13f79f41c92b4f))
* **deploy:** bump coder to sha-e6ce601 ([f6e6587](https://github.com/vymalo/another-adam-rs/commit/f6e658726e53e673a900f7390691ac0d39e2f042))
* **deploy:** bump coder to sha-ff3babf ([50666b0](https://github.com/vymalo/another-adam-rs/commit/50666b074710eb2fbc02f3e81bbfc21805dd253a))

## [0.7.0](https://github.com/vymalo/another-adam-rs/compare/v0.6.0...v0.7.0) (2026-11-27)


### Features

* **adam-agent:** --version and --help answer before any configuration (ANR-1203) ([#496](https://github.com/vymalo/another-adam-rs/issues/496)) ([5ac1037](https://github.com/vymalo/another-adam-rs/commit/5ac1037797ea447bcd032b747a3c0e7629a0d70d))
* **adam-llm-agent:** a session's tools and deny_tools (ADR 0054 slice 3) ([#501](https://github.com/vymalo/another-adam-rs/issues/501)) ([d328128](https://github.com/vymalo/another-adam-rs/commit/d3281289aa6c63e9125f44d19176d367cf26da97))
* **assembly:** the system prompt documents the session tools (ANR-208) ([#499](https://github.com/vymalo/another-adam-rs/issues/499)) ([d142a6e](https://github.com/vymalo/another-adam-rs/commit/d142a6ee3e3122502f2f7e9381fc5929ec17875b))
* **deploy:** generate the turn key in the cluster and push it to AWS (ANR-1305) ([#507](https://github.com/vymalo/another-adam-rs/issues/507)) ([5883708](https://github.com/vymalo/another-adam-rs/commit/588370857dd8f1db3ab878e7ab41631d8e43f8ca))
* **hooks:** the adam-hooks/1 adapter, a program on stdio (ADR 0050 P1) ([#502](https://github.com/vymalo/another-adam-rs/issues/502)) ([d73cc74](https://github.com/vymalo/another-adam-rs/commit/d73cc741dc0b0942a1a8a8c126b9fc63541b147b))
* **llm-agent:** a session's report matches its report_schema (ANR-220) ([#503](https://github.com/vymalo/another-adam-rs/issues/503)) ([e31cd54](https://github.com/vymalo/another-adam-rs/commit/e31cd54e6f443dd67b636ae881365faae46c8ab6))
* **mcp:** add initial MCP server configuration ([1116856](https://github.com/vymalo/another-adam-rs/commit/11168568ce64302b1d7a811625168fb85204c0c8))
* **memory:** the L1 SiMemory port, its testkit and the in-folder adapter ([#497](https://github.com/vymalo/another-adam-rs/issues/497)) ([6fe5642](https://github.com/vymalo/another-adam-rs/commit/6fe56423a001c41775180b9a96025117aa453830))
* **sessions:** a deleted session is gone from every listing and every tool ([#498](https://github.com/vymalo/another-adam-rs/issues/498)) ([a96fdea](https://github.com/vymalo/another-adam-rs/commit/a96fdea7b32a872a563abfec03a19a5bd9d4757f))
* **sessions:** a session's instructions follow the folder's, under a heading ([#500](https://github.com/vymalo/another-adam-rs/issues/500)) ([f4fea7b](https://github.com/vymalo/another-adam-rs/commit/f4fea7b121005908605bba986ece3a9f354c4b65))
* **store-postgres:** the claim is a database function, schema version 9 (ADR 0082 P2) ([#494](https://github.com/vymalo/another-adam-rs/issues/494)) ([59d36b9](https://github.com/vymalo/another-adam-rs/commit/59d36b96fb44550ae2e1a4819c90c203c205139c))
* **store-postgres:** the served document is a virtual column, schema version 8 (ADR 0082 P1) ([#482](https://github.com/vymalo/another-adam-rs/issues/482)) ([56c47a6](https://github.com/vymalo/another-adam-rs/commit/56c47a6280f5d245e5825145b53f25fd34445816))


### Bug Fixes

* **adam-runtime:** name the deleted header flag in the literals slice 5a left behind ([#508](https://github.com/vymalo/another-adam-rs/issues/508)) ([f0a32f9](https://github.com/vymalo/another-adam-rs/commit/f0a32f983781b1b54241db6f0b49dd4dd60f3c62))
* **ci:** release.yml reads no unused field, so actionlint is green again ([#512](https://github.com/vymalo/another-adam-rs/issues/512)) ([749de8b](https://github.com/vymalo/another-adam-rs/commit/749de8b0d0d57721fe026fcb4403a888506dbe27))
* **ci:** the release commit keeps its coder.yml run and the gate reads the image job ([#493](https://github.com/vymalo/another-adam-rs/issues/493)) ([03e7bcd](https://github.com/vymalo/another-adam-rs/commit/03e7bcda64e02a7f03b1f78d8d8dcbaf5e146d7c))
* **coder:** a publication made while the server is indexing is not final (ANR-1185) ([#484](https://github.com/vymalo/another-adam-rs/issues/484)) ([af7459f](https://github.com/vymalo/another-adam-rs/commit/af7459f33d262355535c4da9f5353dedd0303233))
* **docs:** pin mermaid 11.17.2, so the site's diagrams render again (ANR-1189) ([#492](https://github.com/vymalo/another-adam-rs/issues/492)) ([38058c5](https://github.com/vymalo/another-adam-rs/commit/38058c5da637ccc2acd7cfba8e7e4d02e061cf27))
* **memory:** record and observe refuse another scope's file ([#506](https://github.com/vymalo/another-adam-rs/issues/506)) ([301df16](https://github.com/vymalo/another-adam-rs/commit/301df16b73de0827fdbd7cb68fca669bc7ade2b7))
* **runtime:** the session test fixture names the deleted field ([#498](https://github.com/vymalo/another-adam-rs/issues/498) missed it) ([#511](https://github.com/vymalo/another-adam-rs/issues/511)) ([21cdf8c](https://github.com/vymalo/another-adam-rs/commit/21cdf8cdd5d9de160d7786b6fd99b5104deb23c3))
* **sessions:** delete implies cancel, so a working session is deleted ([#505](https://github.com/vymalo/another-adam-rs/issues/505)) ([f7d7cd8](https://github.com/vymalo/another-adam-rs/commit/f7d7cd813d12b02a47a9f647ea9999eba6572a0e))


### Documentation

* **adr:** 0085, an agent keeps the network tier a message names ([#514](https://github.com/vymalo/another-adam-rs/issues/514)) ([5be7a4c](https://github.com/vymalo/another-adam-rs/commit/5be7a4c0ceb59406c650dc8336eccfbc3520ead9))
* **adr:** ADR 0076 is accepted, with the owner's seven answers ([#513](https://github.com/vymalo/another-adam-rs/issues/513)) ([48a0088](https://github.com/vymalo/another-adam-rs/commit/48a0088b31c00e676441ce91441ca329a501acaa))
* **adr:** isolation is a port, with the coder as one adapter (ANR-1288) ([#516](https://github.com/vymalo/another-adam-rs/issues/516)) ([2ff069b](https://github.com/vymalo/another-adam-rs/commit/2ff069b2c9fbb8af08f868cd82fea3035eeb7622))
* **guides:** the long detail sits in "In depth" blocks (ANR-1188) ([#489](https://github.com/vymalo/another-adam-rs/issues/489)) ([4195974](https://github.com/vymalo/another-adam-rs/commit/419597404346fb655b8dea02630a35e72013ab5a))
* **hooks:** plugins: and audit in the agent-folder docs and skill (ANR-699) ([#515](https://github.com/vymalo/another-adam-rs/issues/515)) ([c4afee3](https://github.com/vymalo/another-adam-rs/commit/c4afee3362cffc1c9cb06b36633aaff34510a7d3))
* **reference:** the parity table uses the shared status legend (ANR-1187) ([#488](https://github.com/vymalo/another-adam-rs/issues/488)) ([71ec680](https://github.com/vymalo/another-adam-rs/commit/71ec6801c0718edbfe0444b041621446e188b55a))
* **site:** the conventions v3 and the shared theme pieces (ANR-1086) ([#486](https://github.com/vymalo/another-adam-rs/issues/486)) ([bd96ca1](https://github.com/vymalo/another-adam-rs/commit/bd96ca1586a43b363cd976c5496c42f494ced26c))


### Tests

* **coder:** the LSP indexing tests assert the shape, not the runner's seconds ([#510](https://github.com/vymalo/another-adam-rs/issues/510)) ([4d10caf](https://github.com/vymalo/another-adam-rs/commit/4d10caf965b27bed227697272bad8b5e853488f9))
* **docs-check:** fail on an unclosed ::: in-depth, as the plugin's comment says ([#491](https://github.com/vymalo/another-adam-rs/issues/491)) ([f53dd04](https://github.com/vymalo/another-adam-rs/commit/f53dd04c3b387a276bd53d916a86575e218e80a6))


### Continuous Integration

* **docs:** run the in-depth container's tests, as its header says ([#490](https://github.com/vymalo/another-adam-rs/issues/490)) ([e042b78](https://github.com/vymalo/another-adam-rs/commit/e042b78d7f0f7d0fcd2ae8f5ef4d75cdc3fa8026))


### Chores

* **deploy:** bump coder to sha-38058c5 ([ed611c5](https://github.com/vymalo/another-adam-rs/commit/ed611c54eb779054a6a0abc146b4120e6fa618be))
* **deploy:** bump coder to sha-59d36b9 ([34e38dc](https://github.com/vymalo/another-adam-rs/commit/34e38dccc1d88553ccd32864cef3606a9f78ab1c))
* **deploy:** bump coder to sha-b8809e5 ([f2de1cb](https://github.com/vymalo/another-adam-rs/commit/f2de1cbe454b8cd6d167f11a9ca01a075876b501))

## [0.6.0](https://github.com/vymalo/another-adam-rs/compare/v0.5.0...v0.6.0) (2026-11-27)


### Features

* **a2a:** agent-mode/v1 — folder modes, the extension, activation ([#465](https://github.com/vymalo/another-adam-rs/issues/465)) ([2c2b551](https://github.com/vymalo/another-adam-rs/commit/2c2b551d8887f5cef3c07f48f0140a72ad2cd9ad))
* **adam-engine:** approvals: a host's hook asks and the host answers yes or no (ADR 0047 slice 5) ([#479](https://github.com/vymalo/another-adam-rs/issues/479)) ([6608124](https://github.com/vymalo/another-adam-rs/commit/660812488fcb1eda6d6f69e203fdb43bf2b0d6ca))
* **adam-llm-agent:** a hook's Ask parks the run for the person's approval (ADR 0050, slice H8) ([#478](https://github.com/vymalo/another-adam-rs/issues/478)) ([75ea531](https://github.com/vymalo/another-adam-rs/commit/75ea5315f8dd68b2711e9505fa496866f7f62bea))
* **service:** serve the activity route on a listener of its own, with its own tokens (ADR 0083) ([#474](https://github.com/vymalo/another-adam-rs/issues/474)) ([707d597](https://github.com/vymalo/another-adam-rs/commit/707d597c64924037e8fbf9a5b536c5b0802e810e))
* **store-postgres:** PostgreSQL 18 is the floor, schema version 7 and the AD SQLSTATEs (ADR 0082 P0) ([#476](https://github.com/vymalo/another-adam-rs/issues/476)) ([be80064](https://github.com/vymalo/another-adam-rs/commit/be8006477e2051592c9e8a3e64dbdafa0811e0e5))


### Bug Fixes

* **release:** the release sync renders the chart goldens again, so their checksums move with the version ([#480](https://github.com/vymalo/another-adam-rs/issues/480)) ([b8809e5](https://github.com/vymalo/another-adam-rs/commit/b8809e5f2ad9ed02fe3f6ac62fc3fbeed6869778))


### Documentation

* **skills:** the adam-a2a-extensions skill lists agent-mode/v1 ([#481](https://github.com/vymalo/another-adam-rs/issues/481)) ([baefe11](https://github.com/vymalo/another-adam-rs/commit/baefe118de56c88f925c198076be5259cedfb957))


### Continuous Integration

* **release:** release-please, signed coder images X.Y.Z and the chart as OCI on a tag, and commit lint (ADR 0084) ([#475](https://github.com/vymalo/another-adam-rs/issues/475)) ([b5d1c7e](https://github.com/vymalo/another-adam-rs/commit/b5d1c7eb56e6162348ac94495be1ba400b7a54eb))


### Chores

* **deploy:** bump coder to sha-2c2b551 ([908f3f1](https://github.com/vymalo/another-adam-rs/commit/908f3f13c734c02078ed037ed04c2529ffc43790))
* **deploy:** bump coder to sha-6608124 ([e8809b4](https://github.com/vymalo/another-adam-rs/commit/e8809b4d228126e076cfc5ba491adb98378d2ed0))
* **deploy:** bump coder to sha-75ea531 ([41e49b0](https://github.com/vymalo/another-adam-rs/commit/41e49b0f37e85666ecbcab068fbee7cff707af80))
* **deploy:** bump coder to sha-b5d1c7e ([14c8bcd](https://github.com/vymalo/another-adam-rs/commit/14c8bcd46e45055f86ffa15d50b59c5cf5d82725))
* **deploy:** bump coder to sha-be80064 ([78ae145](https://github.com/vymalo/another-adam-rs/commit/78ae145c79605f3efa7910ecc9a297d323ed6811))
* **deploy:** bump coder to sha-ce99d98 ([71ea0bf](https://github.com/vymalo/another-adam-rs/commit/71ea0bf70697b6b739b3de8939b8acff02e9498e))
