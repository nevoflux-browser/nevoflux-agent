// Run the shop engine under a minimal DOM shim; exit 1 if any page throws.
"use strict";
const path = require("path").resolve(__dirname, "..", "sites") + "/";
function node(tag) {
  return {
    tag, attrs: {}, kids: [], dataset: {}, listeners: {}, textContent: "", value: "",
    setAttribute(k, v) { this.attrs[k] = v; },
    append(...k) { this.kids.push(...k); },
    appendChild(k) { this.kids.push(k); },
    prepend(k) { this.kids.unshift(k); },
    addEventListener(e, f) { this.listeners[e] = f; },
    remove() {}, querySelector() { return { value: "standard" }; },
  };
}
for (const [site, page, query] of [["shop", "product", "?sku=K-2"], ["shop", "search", "?q=a&page=2"],
  ["shop", "index", ""], ["shop", "cart", ""], ["shop", "checkout", ""], ["zh-shop", "product", "?sku=Z-3"]]) {
  const main = node("main");
  const body = node("body");
  body.dataset.page = page;
  global.window = {};
  global.document = { createElement: node, getElementById: (id) => (id === "main" ? main : null), body };
  global.location = { search: query, href: "" };
  global.localStorage = { getItem: () => null, setItem() {} };
  global.nfEvent = () => {};
  try {
    delete require.cache[require.resolve(path + site + "/data.js")];
    delete require.cache[require.resolve(path + "shop/app.js")];
    require(path + site + "/data.js");
    require(path + "shop/app.js");
    console.log(site, page, "ok, main children:", main.kids.length);
  } catch (e) {
    console.log(site, page, "THROWS:", e.message);
    process.exitCode = 1;
  }
}
