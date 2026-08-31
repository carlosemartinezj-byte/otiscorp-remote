const http = require("http");
const fs = require("fs");
const path = require("path");
const root = path.join(__dirname, "ui");
const types = { ".html":"text/html", ".css":"text/css", ".js":"text/javascript", ".ico":"image/x-icon" };
http.createServer((req, res) => {
  let p = decodeURIComponent(req.url.split("?")[0]);
  if (p === "/") p = "/index.html";
  const f = path.join(root, p);
  fs.readFile(f, (e, data) => {
    if (e) { res.writeHead(404); res.end("404"); return; }
    res.writeHead(200, { "Content-Type": types[path.extname(f)] || "application/octet-stream" });
    res.end(data);
  });
}).listen(4820, () => console.log("preview http://localhost:4820"));
