"use strict";
const CITIES = ["Seoul", "Shanghai", "Shenzhen", "Singapore", "Sydney", "Stockholm"];
const input = document.getElementById("city");
const list = document.getElementById("list");
let timer = null;
input.addEventListener("input", () => {
  clearTimeout(timer);
  list.hidden = true;
  input.setAttribute("aria-expanded", "false");
  const q = input.value.trim().toLowerCase();
  if (!q) return;
  // Options appear late on purpose, like a real remote suggestion list.
  timer = setTimeout(() => {
    list.textContent = "";
    for (const c of CITIES.filter((c) => c.toLowerCase().startsWith(q))) {
      const li = document.createElement("li");
      li.setAttribute("role", "option");
      li.textContent = c;
      li.addEventListener("click", () => {
        input.value = c;
        list.hidden = true;
        document.getElementById("status").textContent = "Selected " + c;
        nfEvent("widgets", "combo_pick", { value: c });
      });
      list.appendChild(li);
    }
    list.hidden = false;
    input.setAttribute("aria-expanded", "true");
  }, 150);
});
