// Applies a manually chosen theme before the first paint; without a choice the system theme wins.
try {
  var t = localStorage.getItem("tugport-theme");
  if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
} catch (e) {}
