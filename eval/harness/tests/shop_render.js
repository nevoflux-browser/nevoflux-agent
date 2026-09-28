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

// The cart must let a line be removed (a task asks to swap one product for
// another; without a remove control the agent could only fake it).
{
  const main = node("main");
  const body = node("body");
  body.dataset.page = "cart";
  let saved = null;
  const events = [];
  global.window = {};
  global.document = { createElement: node, getElementById: (id) => (id === "main" ? main : null), body };
  global.location = { search: "", href: "", reload() {} };
  global.localStorage = { getItem: () => '[{"sku":"Z-1","qty":1}]', setItem(k, v) { saved = v; } };
  global.nfEvent = (site, kind, data) => events.push([site, kind, data]);
  delete require.cache[require.resolve(path + "zh-shop/data.js")];
  delete require.cache[require.resolve(path + "shop/app.js")];
  require(path + "zh-shop/data.js");
  require(path + "shop/app.js");
  const find = (n) => (n && n.tag === "button" && n.attrs["data-sku"] === "Z-1" ? n
    : (n && n.kids || []).map(find).find(Boolean));
  const button = find(main);
  if (!button || !button.listeners.click) {
    console.log("cart: no remove button for Z-1");
    process.exitCode = 1;
  } else {
    button.listeners.click();
    const ok = saved === "[]" && events.some(([s, k, d]) => s === "zh-shop" && k === "cart_remove" && d.sku === "Z-1");
    console.log("cart remove", ok ? "ok" : "BROKEN", saved, JSON.stringify(events));
    if (!ok) process.exitCode = 1;
  }
}
