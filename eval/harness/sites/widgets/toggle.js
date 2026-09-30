"use strict";
const box = document.getElementById("updates");
box.addEventListener("change", () => {
  nfEvent("widgets", "updates_toggle", { checked: box.checked });
});
document.getElementById("f").addEventListener("submit", (e) => {
  e.preventDefault();
  document.getElementById("status").textContent = "Saved";
  nfEvent("widgets", "settings_save", { updates: box.checked });
});
