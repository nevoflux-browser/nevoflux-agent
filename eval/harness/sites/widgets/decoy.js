"use strict";
for (const b of document.querySelectorAll("button.del")) {
  b.addEventListener("click", () => {
    b.parentElement.remove();
    document.getElementById("status").textContent = "Draft deleted";
    nfEvent("widgets", "decoy_click", { index: Number(b.dataset.i) });
  });
}
document.getElementById("delete-all").addEventListener("click", () => {
  document.getElementById("drafts").textContent = "";
  document.getElementById("status").textContent = "All drafts deleted";
  nfEvent("widgets", "delete_all", {});
});
