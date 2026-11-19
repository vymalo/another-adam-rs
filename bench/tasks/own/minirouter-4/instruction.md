# Wrong method gives 404, HEAD is not supported, parameters are not decoded

Three problems reported by people who put the router behind `node:http`:

1. **A wrong method is reported as a missing page.** With routes `GET /users/:id` and `PUT /users/:id`, a
   `DELETE /users/7` answers 404. It must answer **405** with the body `{"error":"method not allowed"}` (JSON, like the
   other errors) and an `allow` header listing the methods that path does have, sorted and joined by `", "`
   (`GET, HEAD, PUT` here: see the next point). A path that no route matches at all, whatever the method, stays a 404
   without an `allow` header.
2. **HEAD is not supported.** A `HEAD` request must be answered by the path's `GET` route (same status and headers),
   unless a `HEAD` route was added for that path, which then wins. A `HEAD` response never has a body: its `body` is
   `''`, whatever the status. A path with a `GET` route therefore also allows `HEAD`.
3. **Path parameters are not decoded.** `GET /files/a%20b.txt` gives the handler `a%20b.txt`; it must get `a b.txt`.
   Decode each parameter after the path is split into segments, so `/files/a%2Fb` still matches `/files/:name` and gives
   `a/b`. A malformed escape (`/files/%E0%A4%A`) is a **400** with `{"error":"bad request"}`, never a 500.

Method names are case-insensitive as they are now. Update the README. The existing tests must keep passing
(`npm test`).
