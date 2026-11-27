---
aside: false
outline: false
---

<script setup>
import { useA2aServerSpec } from "../.vitepress/openapi/client";

useA2aServerSpec();
</script>

# A2A server API

Every operation the A2A server answers, on both bindings, rendered from [a2a-server.openapi.json](a2a-server.openapi.json)
(OpenAPI 3.1) at build time. How a client uses them (follow-ups, push notifications, the streams, the extended card) is in
[the A2A server reference](../reference/a2a-server.md).

<OASpec />
