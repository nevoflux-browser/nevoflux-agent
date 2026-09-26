"use strict";
const banner = document.getElementById("cookie-banner");
banner.addEventListener("click", (e) => {
  if (e.target === banner) nfEvent("widgets", "overlay_click", {});
});
document.getElementById("accept").addEventListener("click", () => {
  banner.remove();
  nfEvent("widgets", "cookie_accept", {});
});
document.getElementById("target").addEventListener("click", () => {
  document.getElementById("status").textContent = "Settings saved";
  nfEvent("widgets", "target_click", {});
});
