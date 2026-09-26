"use strict";
// Shared shop engine for `shop` and `zh-shop`; each site supplies data.js
// (SITE, L10N, PRODUCTS). The page kind comes from <body data-page>.
(function () {
  const L = window.L10N;
  const SITE = window.SITE;
  const PAGE_SIZE = 5;
  const CART_KEY = SITE + "-cart";
  const params = new URLSearchParams(location.search);
  const main = document.getElementById("main");

  function el(tag, attrs, ...kids) {
    const n = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) n.setAttribute(k, v);
    for (const k of kids) n.append(k);
    return n;
  }
  const money = (p) => L.currency + p.toFixed(2);
  const bySku = (sku) => window.PRODUCTS.find((p) => p.sku === sku);
  function cart() {
    try { return JSON.parse(localStorage.getItem(CART_KEY) || "[]"); } catch (e) { return []; }
  }
  const saveCart = (c) => localStorage.setItem(CART_KEY, JSON.stringify(c));

  function productRow(p) {
    return el("li", {},
      el("a", { href: "product.html?sku=" + encodeURIComponent(p.sku) }, p.name),
      " — " + money(p) + " · " + L.rating + " " + p.rating + " (" + p.reviews + " " + L.reviews + ")");
  }

  function searchBox(q) {
    const form = el("form", { id: "search-form", role: "search" });
    const input = el("input", { id: "q", name: "q", "aria-label": L.searchPlaceholder,
      placeholder: L.searchPlaceholder });
    input.value = q || "";
    form.append(input, " ", el("button", { type: "submit" }, L.search));
    form.addEventListener("submit", (e) => {
      e.preventDefault();
      const value = input.value.trim();
      nfEvent(SITE, "search", { q: value });
      location.href = "search.html?q=" + encodeURIComponent(value);
    });
    return form;
  }

  const pages = {
    index() {
      main.append(searchBox(""), el("h2", {}, L.featured));
      const ul = el("ul", { id: "products" });
      window.PRODUCTS.forEach((p) => ul.append(productRow(p)));
      main.append(ul);
    },
    search() {
      const q = params.get("q") || "";
      const page = Math.max(1, parseInt(params.get("page") || "1", 10));
      const hits = window.PRODUCTS.filter((p) => p.name.toLowerCase().includes(q.toLowerCase()));
      const total = Math.max(1, Math.ceil(hits.length / PAGE_SIZE));
      main.append(searchBox(q), el("h2", {}, L.results + " “" + q + "” — " + L.page + " " + page + "/" + total));
      const ul = el("ul", { id: "results" });
      hits.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE).forEach((p) => ul.append(productRow(p)));
      if (!hits.length) ul.append(el("li", {}, L.noResults));
      main.append(ul);
      const nav = el("nav", { "aria-label": L.page });
      const link = (n, label) => el("a", { href: "search.html?q=" + encodeURIComponent(q) + "&page=" + n }, label);
      if (page > 1) nav.append(link(page - 1, L.prev), " ");
      if (page < total) nav.append(link(page + 1, L.next));
      main.append(nav);
    },
    product() {
      const p = bySku(params.get("sku"));
      if (!p) { main.append(el("p", {}, L.noResults)); return; }
      const qty = el("input", { id: "qty", type: "number", min: "1", value: "1", "aria-label": L.qty });
      const add = el("button", { id: "add" }, L.addToCart);
      const status = el("p", { id: "status", role: "status" });
      add.addEventListener("click", () => {
        const n = Math.max(1, parseInt(qty.value || "1", 10));
        const c = cart();
        const line = c.find((x) => x.sku === p.sku);
        if (line) line.qty += n; else c.push({ sku: p.sku, qty: n });
        saveCart(c);
        status.textContent = L.added;
        nfEvent(SITE, "cart_add", { sku: p.sku, qty: n });
      });
      main.append(el("h1", {}, p.name), el("p", { id: "price" }, money(p)),
        el("p", {}, L.rating + " " + p.rating + " (" + p.reviews + " " + L.reviews + ")"),
        el("p", {}, L.warranty + ": " + p.warranty + " " + L.months),
        el("label", {}, L.qty + " ", qty), " ", add, status,
        el("p", {}, el("a", { href: "cart.html" }, L.cart)));
    },
    cart() {
      const c = cart();
      main.append(el("h1", {}, L.cart));
      if (!c.length) { main.append(el("p", {}, L.emptyCart)); return; }
      const ul = el("ul", { id: "cart" });
      c.forEach((x) => ul.append(el("li", {}, bySku(x.sku).name + " × " + x.qty)));
      main.append(ul, el("p", {}, el("a", { href: "checkout.html", id: "to-checkout" }, L.checkout)));
    },
    checkout() {
      const c = cart();
      main.append(el("h1", {}, L.checkout));
      if (!c.length) { main.append(el("p", {}, L.emptyCart)); return; }
      const form = el("form", { id: "checkout-form" });
      const name = el("input", { id: "name", name: "name", "aria-label": L.name, required: "" });
      const address = el("input", { id: "address", name: "address", "aria-label": L.address, required: "" });
      const std = el("input", { type: "radio", name: "ship", value: "standard", checked: "" });
      const exp = el("input", { type: "radio", name: "ship", value: "express" });
      form.append(
        el("p", {}, el("label", {}, L.name + " ", name)),
        el("p", {}, el("label", {}, L.address + " ", address)),
        el("fieldset", {}, el("legend", {}, L.delivery),
          el("label", {}, std, " " + L.standard), " ", el("label", {}, exp, " " + L.express)),
        el("button", { type: "submit" }, L.placeOrder));
      const status = el("p", { id: "status", role: "status" });
      form.addEventListener("submit", (e) => {
        e.preventDefault();
        const ship = form.querySelector("input[name=ship]:checked").value;
        nfEvent(SITE, "order", { sku_list: c.map((x) => x.sku), name: name.value.trim(),
          address: address.value.trim(), ship: ship });
        saveCart([]);
        form.remove();
        status.textContent = L.ordered;
      });
      main.append(form, status);
    },
  };

  const header = el("header", {}, el("a", { href: "index.html" }, "Home"), " · ",
    el("a", { href: "cart.html" }, L.cart));
  document.body.prepend(header);
  pages[document.body.dataset.page]();
})();
