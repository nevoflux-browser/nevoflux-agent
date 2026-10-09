/* jev2 ops portal: file a report; the beacon records what was submitted. */
"use strict";
document.getElementById("report").addEventListener("submit", function (ev) {
  ev.preventDefault();
  var f = ev.target;
  var data = { service: f.service.value.trim(), count: f.count.value.trim(), incident: f.incident.value.trim() };
  nfEvent(f.dataset.site, "report", data);
  var done = document.getElementById("done");
  done.textContent = done.dataset.msg;
});
