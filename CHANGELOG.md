# Changelog

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
