// Apply the saved theme before first paint. "tm-theme" holds light/dark/coffee.
(function () {
  var t = null;
  try { t = localStorage.getItem('tm-theme'); } catch (e) {}
  if (t !== 'light' && t !== 'dark' && t !== 'coffee') t = matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  document.documentElement.setAttribute('data-tm-theme', t);
})();
