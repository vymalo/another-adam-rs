// Client side of the API page. Only api/a2a-server.md imports this, so the document (a few hundred KB of JSON) is in
// that page's chunk and not in the theme that every page loads.
import { useOpenapi } from "vitepress-openapi/client";
import spec from "../../api/a2a-server.openapi.json?raw";

// Makes the document the global spec of the library's components. The theme holds the rest of its configuration.
export function useA2aServerSpec() {
  useOpenapi({ spec });
}
