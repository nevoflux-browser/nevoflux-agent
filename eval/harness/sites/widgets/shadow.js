"use strict";
class PromoBox extends HTMLElement {
  constructor() {
    super();
    const root = this.attachShadow({ mode: "open" });
    const label = document.createElement("label");
    label.textContent = "Promo code ";
    const input = document.createElement("input");
    input.setAttribute("aria-label", "Promo code");
    label.appendChild(input);
    const button = document.createElement("button");
    button.textContent = "Apply code";
    button.addEventListener("click", () => {
      const value = input.value.trim();
      document.getElementById("status").textContent = "Applied " + value;
      nfEvent("widgets", "shadow_submit", { value: value });
    });
    root.append(label, " ", button);
  }
}
customElements.define("promo-box", PromoBox);
