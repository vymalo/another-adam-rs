import type { Theme } from "vitepress";
import Md3Theme from "@vaam-apps/vitepress-md3";
import { theme as openapiTheme, useOpenapi } from "vitepress-openapi/client";
import "vitepress-openapi/dist/style.css";

// Only the configuration of the library is here; the document itself is loaded by the API page (openapi/client.ts).
const theme: Theme = {
  ...Md3Theme,
  async enhanceApp(ctx) {
    await Md3Theme.enhanceApp?.(ctx);
    useOpenapi({
      // No "Try it out": the docs site is not a client of any deployment, and the document's server is a relative `.`.
      config: { operation: { hiddenSlots: ["playground"] } },
    });
    await openapiTheme.enhanceApp(ctx);
  },
};

export default theme;
