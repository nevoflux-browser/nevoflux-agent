"use strict";
document.getElementById("pay").addEventListener("click", async () => {
  nfEvent("widgets", "pay_click", {});
  document.getElementById("status").textContent = "Processing…";
  await fetch("/__slow?ms=1500&kind=pay_done&site=widgets");
  document.getElementById("status").textContent = "Paid";
});
