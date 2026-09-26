"use strict";
document.getElementById("f").addEventListener("submit", (e) => {
  e.preventDefault();
  const data = {
    size: document.getElementById("size").value,
    gift: document.getElementById("gift").checked,
    ship: document.querySelector("input[name=ship]:checked").value,
  };
  document.getElementById("status").textContent = "Order placed";
  nfEvent("widgets", "form_submit", data);
});
