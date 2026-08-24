(function () {
  if (typeof document === "undefined") return;

  var appearance = document.querySelector("[data-appearance-toggle]");
  if (appearance) {
    var appearanceLabel = appearance.dataset.appearanceLabel || "Theme";

    function applyAppearance(choice, persist) {
      var dark = choice === "dark" || (choice === "auto" && matchMedia("(prefers-color-scheme: dark)").matches);
      var action = dark ? "light" : "dark";
      document.documentElement.dataset.themeChoice = choice;
      document.documentElement.dataset.theme = dark ? "dark" : "light";
      appearance.setAttribute("aria-label", appearanceLabel + ": switch to " + action + " theme");
      appearance.title = "Switch to " + action + " theme";
      if (persist) {
        try { localStorage.setItem("zola-docs-theme", choice); } catch (_error) {}
      }
    }

    applyAppearance(document.documentElement.dataset.themeChoice || "auto", false);
    appearance.addEventListener("click", function () {
      var choice = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
      applyAppearance(choice, true);
    });
    matchMedia("(prefers-color-scheme: dark)").addEventListener("change", function () {
      if (document.documentElement.dataset.themeChoice === "auto") applyAppearance("auto", false);
    });
  }

  var tableOfContents = document.querySelector(".toc");
  if (tableOfContents) {
    var compactTableOfContents = matchMedia("(max-width: 52rem)");
    function updateTableOfContents() {
      tableOfContents.open = !compactTableOfContents.matches;
    }
    compactTableOfContents.addEventListener("change", updateTableOfContents);
    updateTableOfContents();
  }

  var navigationButton = document.querySelector("[data-navigation-open]");
  var sidebar = document.querySelector(".sidebar");
  var sidebarDetails = sidebar && sidebar.querySelector(":scope > details");
  var navigation = sidebarDetails && sidebarDetails.querySelector(":scope > nav");
  if (navigationButton && sidebar && navigation) {
    var mobile = matchMedia("(max-width: 52rem)");
    var dialog = document.createElement("dialog");
    var dialogHeader = document.createElement("div");
    var dialogTitle = document.createElement("strong");
    var closeButton = document.createElement("button");
    var groupState = [];
    dialog.id = "mobile-navigation-dialog";
    dialog.className = "navigation-dialog";
    dialog.setAttribute("aria-label", navigation.getAttribute("aria-label") || "Navigation");
    dialogHeader.className = "navigation-dialog-header";
    dialogTitle.textContent = navigation.getAttribute("aria-label") || "Navigation";
    closeButton.className = "navigation-dialog-close";
    closeButton.type = "button";
    closeButton.textContent = "Close";
    dialogHeader.append(dialogTitle, closeButton);
    dialog.append(dialogHeader);
    document.body.append(dialog);

    function enhanceGroups() {
      navigation.querySelectorAll(":scope > section").forEach(function (section) {
        var heading = section.querySelector(":scope > h2");
        var list = section.querySelector(":scope > ul");
        if (!heading || !list) return;
        var group = document.createElement("details");
        var summary = document.createElement("summary");
        var headingLink = heading.querySelector("a");
        var rootItem;
        group.className = "mobile-navigation-group";
        group.open = Boolean(section.querySelector('[aria-current="page"]'));
        summary.textContent = heading.textContent;
        if (headingLink) {
          rootItem = document.createElement("li");
          rootItem.className = "mobile-navigation-root";
          rootItem.append(headingLink.cloneNode(true));
          list.prepend(rootItem);
        }
        group.append(summary, list);
        heading.replaceWith(group);
        groupState.push({ section: section, heading: heading, list: list, group: group, rootItem: rootItem });
      });
    }

    function restoreGroups() {
      groupState.forEach(function (entry) {
        if (entry.rootItem) entry.rootItem.remove();
        entry.group.replaceWith(entry.heading);
        entry.heading.after(entry.list);
      });
      groupState = [];
    }

    function closeNavigation() {
      if (dialog.open) dialog.close();
    }

    function updateNavigation() {
      if (mobile.matches) {
        delete sidebar.dataset.navigationDesktop;
        sidebarDetails.open = false;
        if (!dialog.contains(navigation)) {
          dialog.append(navigation);
          enhanceGroups();
        }
        sidebar.dataset.navigationEnhanced = "true";
        navigationButton.hidden = false;
      } else {
        closeNavigation();
        if (!sidebarDetails.contains(navigation)) {
          restoreGroups();
          sidebarDetails.append(navigation);
        }
        delete sidebar.dataset.navigationEnhanced;
        sidebar.dataset.navigationDesktop = "true";
        sidebarDetails.open = true;
        navigationButton.hidden = true;
      }
    }

    navigationButton.addEventListener("click", function () {
      dialog.showModal();
      navigationButton.setAttribute("aria-expanded", "true");
      var current = dialog.querySelector('[aria-current="page"]');
      (current || closeButton).focus();
      if (current) current.scrollIntoView({ block: "center" });
    });
    closeButton.addEventListener("click", closeNavigation);
    dialog.addEventListener("click", function (event) {
      if (event.target === dialog) closeNavigation();
    });
    dialog.addEventListener("close", function () {
      navigationButton.setAttribute("aria-expanded", "false");
      navigationButton.focus();
    });
    mobile.addEventListener("change", updateNavigation);
    updateNavigation();
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
