(function () {
  if (typeof document === "undefined") return;

  var appearance = document.querySelector("[data-appearance-toggle]");
  if (appearance) {
    var icon = appearance.querySelector("[data-appearance-icon]");
    var choices = ["auto", "light", "dark"];

    function applyAppearance(choice, persist) {
      var dark = choice === "dark" || (choice === "auto" && matchMedia("(prefers-color-scheme: dark)").matches);
      document.documentElement.dataset.themeChoice = choice;
      document.documentElement.dataset.theme = dark ? "dark" : "light";
      icon.textContent = choice.charAt(0).toUpperCase() + choice.slice(1);
      appearance.title = "Appearance: " + choice;
      if (persist) {
        try { localStorage.setItem("zola-docs-theme", choice); } catch (_error) {}
      }
    }

    applyAppearance(document.documentElement.dataset.themeChoice || "auto", false);
    appearance.addEventListener("click", function () {
      var current = document.documentElement.dataset.themeChoice || "auto";
      applyAppearance(choices[(choices.indexOf(current) + 1) % choices.length], true);
    });
    matchMedia("(prefers-color-scheme: dark)").addEventListener("change", function () {
      if (document.documentElement.dataset.themeChoice === "auto") applyAppearance("auto", false);
    });
  }

  document.querySelectorAll("[data-version-manifest]").forEach(function (link) {
    link.addEventListener("click", async function (event) {
      event.preventDefault();
      var root = link.dataset.versionRoot;
      try {
        var response = await fetch(link.dataset.versionManifest);
        if (!response.ok) throw new Error("Version route manifest request failed");
        var routes = await response.json();
        var current = link.dataset.currentPath;
        if (routes.includes(current)) {
          location.href = root.replace(/\/$/, "") + current;
          return;
        }
      } catch (_error) {}
      location.href = root;
    });
  });

  document.querySelectorAll("pre:not(.mermaid)").forEach(function (block) {
    var button = document.createElement("button");
    button.className = "copy-button";
    button.type = "button";
    button.textContent = "Copy";
    button.setAttribute("aria-label", "Copy code");
    button.addEventListener("click", async function () {
      var source = block.querySelector("code") || block;
      var text = source.textContent
        .replace(/^\s?[$#]\s+/gm, "")
        .replace(/^\s*\n/gm, "");
      try {
        await navigator.clipboard.writeText(text);
        button.textContent = "Copied";
      } catch (_error) {
        button.textContent = "Copy failed";
      }
      setTimeout(function () { button.textContent = "Copy"; }, 1200);
    });
    block.appendChild(button);
  });
})();
