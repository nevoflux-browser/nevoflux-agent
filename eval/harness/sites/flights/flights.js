"use strict";
const CITIES = ["Shanghai", "Shenzhen", "Singapore", "Seoul", "Sydney", "Beijing", "Bangkok"];

function combo(inputId, listId) {
  const input = document.getElementById(inputId);
  const list = document.getElementById(listId);
  let timer = null;
  input.addEventListener("input", () => {
    clearTimeout(timer);
    list.hidden = true;
    input.dataset.picked = "";
    const q = input.value.trim().toLowerCase();
    if (!q) return;
    // Suggestions arrive late, like a real remote lookup.
    timer = setTimeout(() => {
      list.textContent = "";
      for (const c of CITIES.filter((c) => c.toLowerCase().startsWith(q))) {
        const li = document.createElement("li");
        li.setAttribute("role", "option");
        li.textContent = c;
        li.addEventListener("click", () => {
          input.value = c;
          input.dataset.picked = c;
          list.hidden = true;
          input.setAttribute("aria-expanded", "false");
        });
        list.appendChild(li);
      }
      list.hidden = false;
      input.setAttribute("aria-expanded", "true");
    }, 150);
  });
  // Typing the full name exactly also counts as picking it.
  return () => input.dataset.picked || CITIES.find((c) => c.toLowerCase() === input.value.trim().toLowerCase()) || "";
}

const fromCity = combo("from", "from-list");
const toCity = combo("to", "to-list");
document.getElementById("f").addEventListener("submit", (e) => {
  e.preventDefault();
  const q = {
    from: fromCity(),
    to: toCity(),
    date: document.getElementById("date").value,
    direct: document.getElementById("direct").checked,
  };
  if (!q.from || !q.to || !q.date) {
    document.getElementById("status").textContent = "Pick a city from the list for From and To, and a date.";
    return;
  }
  nfEvent("flights", "flight_search", q);
  const p = new URLSearchParams({ from: q.from, to: q.to, date: q.date, direct: q.direct ? "1" : "0" });
  location.href = "results.html?" + p.toString();
});
