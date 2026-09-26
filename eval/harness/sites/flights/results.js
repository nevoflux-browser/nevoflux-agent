"use strict";
// The same 12 flights for any route and date. Task answers come from here:
// cheapest direct = NF752 ($219); direct under $300 = NF752, NF118;
// three cheapest overall = NF640, NF752, NF866 (only NF752 is direct).
const FLIGHTS = [
  { no: "NF101", dep: "06:10", stops: 0, price: 412 },
  { no: "NF205", dep: "07:30", stops: 1, price: 268 },
  { no: "NF318", dep: "08:45", stops: 0, price: 355 },
  { no: "NF422", dep: "09:20", stops: 1, price: 241 },
  { no: "NF537", dep: "10:05", stops: 0, price: 389 },
  { no: "NF640", dep: "11:40", stops: 2, price: 199 },
  { no: "NF752", dep: "12:15", stops: 0, price: 219 },
  { no: "NF866", dep: "13:50", stops: 1, price: 226 },
  { no: "NF973", dep: "15:30", stops: 0, price: 331 },
  { no: "NF118", dep: "17:05", stops: 0, price: 276 },
  { no: "NF229", dep: "19:40", stops: 1, price: 310 },
  { no: "NF334", dep: "21:55", stops: 0, price: 445 },
];
const PAGE_SIZE = 6;
const params = new URLSearchParams(location.search);
const direct = params.get("direct") === "1";
const page = Math.max(1, parseInt(params.get("page") || "1", 10));
const sorted = params.get("sort") === "price";
let rows = FLIGHTS.filter((f) => !direct || f.stops === 0);
if (sorted) rows = rows.slice().sort((a, b) => a.price - b.price);
const total = Math.max(1, Math.ceil(rows.length / PAGE_SIZE));

document.getElementById("heading").textContent =
  (params.get("from") || "?") + " → " + (params.get("to") || "?") + " on " + (params.get("date") || "?") +
  (direct ? " (direct only)" : "") + " — page " + page + " of " + total;

const tbody = document.querySelector("#results tbody");
for (const f of rows.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE)) {
  const tr = document.createElement("tr");
  for (const v of [f.no, f.dep, f.stops === 0 ? "Direct" : f.stops + " stop" + (f.stops > 1 ? "s" : ""), "$" + f.price]) {
    const td = document.createElement("td");
    td.textContent = v;
    tr.appendChild(td);
  }
  tbody.appendChild(tr);
}

function url(changes) {
  const p = new URLSearchParams(location.search);
  for (const [k, v] of Object.entries(changes)) p.set(k, v);
  return "results.html?" + p.toString();
}
const pager = document.getElementById("pager");
for (let n = 1; n <= total; n++) {
  const a = document.createElement("a");
  a.href = url({ page: n });
  a.textContent = "Page " + n;
  pager.append(a, " ");
}
document.getElementById("sort").addEventListener("click", () => {
  location.href = url({ sort: "price", page: 1 });
});
