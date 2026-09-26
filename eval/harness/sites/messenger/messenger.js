"use strict";
// Contact ids are what tasks check (`to`); the label shows both scripts so
// English and Chinese tasks can name the same person.
const CONTACTS = [
  { id: "Li Lei", label: "Li Lei (李雷)" },
  { id: "Zhang San", label: "Zhang San (张三)" },
  { id: "Han Meimei", label: "Han Meimei (韩梅梅)" },
  { id: "Zhang Sanfeng", label: "Zhang Sanfeng (张三丰)" },
];
const list = document.getElementById("contacts");
const text = document.getElementById("text");
const send = document.getElementById("send");
const chatLog = document.getElementById("history");
let current = null;

for (const c of CONTACTS) {
  const li = document.createElement("li");
  li.setAttribute("role", "option");
  li.setAttribute("aria-selected", "false");
  li.tabIndex = 0;
  li.textContent = c.label;
  li.addEventListener("click", () => {
    current = c;
    for (const x of list.children) x.setAttribute("aria-selected", "false");
    li.setAttribute("aria-selected", "true");
    document.getElementById("chat-title").textContent = "Chat with " + c.label;
    chatLog.textContent = "";
    text.disabled = false;
    send.disabled = false;
  });
  list.appendChild(li);
}

document.getElementById("compose").addEventListener("submit", (e) => {
  e.preventDefault();
  const msg = text.value.trim();
  if (!current || !msg) return;
  const li = document.createElement("li");
  li.textContent = "You: " + msg;
  chatLog.appendChild(li);
  text.value = "";
  nfEvent("messenger", "msg_send", { to: current.id, text: msg });
});
