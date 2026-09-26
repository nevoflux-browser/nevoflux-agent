"use strict";
const ol = document.getElementById("rows");
for (let i = 1; i <= 200; i++) {
  const li = document.createElement("li");
  li.textContent = i === 187 ? "Secret code: ZETA-7731" : "Batch " + (1000 + i) + " checked, no issues";
  ol.appendChild(li);
}
