/* Report an agent-visible action to the eval site server's event log. */
"use strict";
window.nfEvent = function nfEvent(site, kind, data) {
  try {
    fetch("/__event", {
      method: "POST",
      keepalive: true,
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ site: site, kind: kind, data: data || {} }),
    });
  } catch (e) {
    /* never let the beacon break the page under test */
  }
};
