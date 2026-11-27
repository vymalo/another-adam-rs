# Changelog

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
