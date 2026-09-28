"use strict";
document.getElementById("inner").addEventListener("click", () => {
  document.getElementById("status").textContent = "Data refreshed";
  nfEvent("widgets", "iframe_click", {});
});
