// Bonfire client-side helpers, shared by every page (loaded in base.html).
//
// Times: any `<time datetime="..." data-relative>` gets a relative text
// ("5 minutes ago", "3 days ago"...) refreshed while the page is open, and
// its local date and hour as a tooltip. Without JavaScript the text written
// by the server (in UTC) is kept.
//
// Links: any `<button data-share-url="/path" hidden>` is shown and opens the
// device's share menu (phones), or copies the full link to the clipboard.
//
// Long posts: any `<p data-collapsible id="...">` taller than the limit set in
// style.css (`--collapsed-lines`) is cut with a "Show more" button. Without
// JavaScript posts stay complete.

const Bonfire = (() => {
    // Same language as the interface, not the browser, so pages don't mix languages.
    const language = document.documentElement.lang || "en";

    // From the largest unit to the smallest: the first one that fits is used.
    // Months and years are averages, precise enough for "x months ago".
    const UNITS = [
        ["year", 365.25 * 24 * 60 * 60],
        ["month", 30.44 * 24 * 60 * 60],
        ["week", 7 * 24 * 60 * 60],
        ["day", 24 * 60 * 60],
        ["hour", 60 * 60],
        ["minute", 60],
        ["second", 1],
    ];

    const relativeFormat = new Intl.RelativeTimeFormat(language, { numeric: "always" });
    const localFormat = new Intl.DateTimeFormat(language, {
        dateStyle: "full",
        timeStyle: "medium",
    });

    // "5 minutes ago". Dates slightly in the future (clocks not in sync) count as now.
    function formatRelative(date, now = new Date()) {
        const seconds = Math.max(0, (now - date) / 1000);
        for (const [unit, unitSeconds] of UNITS) {
            if (seconds >= unitSeconds || unit === "second") {
                return relativeFormat.format(-Math.floor(seconds / unitSeconds), unit);
            }
        }
    }

    // "Friday, October 30, 2026 at 6:45:12 PM", in the viewer's time zone.
    function formatLocal(date) {
        return localFormat.format(date);
    }

    // Updates every relative time inside `root`.
    function refreshTimes(root = document) {
        const now = new Date();
        for (const element of root.querySelectorAll("time[datetime][data-relative]")) {
            const date = new Date(element.getAttribute("datetime"));
            if (Number.isNaN(date.getTime())) {
                continue;
            }
            element.textContent = formatRelative(date, now);
            element.title = formatLocal(date);
        }
    }

    // Full URL of a path on this site.
    function absoluteUrl(path) {
        return new URL(path, window.location.origin).href;
    }

    async function shareLink(button) {
        const url = absoluteUrl(button.dataset.shareUrl);
        if (navigator.share) {
            try {
                await navigator.share({ url });
            } catch {
                // Closed by the user, nothing to do.
            }
            return;
        }
        const label = button.textContent;
        try {
            await navigator.clipboard.writeText(url);
            button.textContent = "Link copied";
        } catch {
            // Clipboard refused (permissions, or not HTTPS): show the link to copy by hand.
            window.prompt("Copy this link:", url);
            return;
        }
        setTimeout(() => {
            button.textContent = label;
        }, 2000);
    }

    // Shows and activates the share buttons inside `root`, once each.
    function setupShareButtons(root = document) {
        for (const button of root.querySelectorAll("button[data-share-url]")) {
            if (button.dataset.shareReady) {
                continue;
            }
            button.dataset.shareReady = "true";
            if (navigator.share) {
                button.textContent = "Share link";
            }
            button.hidden = false;
            button.addEventListener("click", () => shareLink(button));
        }
    }

    // Collapses the long texts inside `root`, once each.
    function setupCollapsible(root = document) {
        for (const text of root.querySelectorAll("[data-collapsible]")) {
            if (text.dataset.collapseReady) {
                continue;
            }
            text.dataset.collapseReady = "true";
            text.classList.add("collapsed");
            // Short enough: nothing hidden, no button.
            if (text.scrollHeight <= text.clientHeight + 1) {
                text.classList.remove("collapsed");
                continue;
            }

            const button = document.createElement("button");
            button.type = "button";
            button.className = "action show-more";
            button.textContent = "Show more";
            button.setAttribute("aria-expanded", "false");
            button.setAttribute("aria-controls", text.id);
            button.addEventListener("click", () => {
                const collapsed = text.classList.toggle("collapsed");
                button.textContent = collapsed ? "Show more" : "Show less";
                button.setAttribute("aria-expanded", String(!collapsed));
            });
            text.after(button);
        }
    }

    function start() {
        refreshTimes();
        setupShareButtons();
        setupCollapsible();
        // Often enough for "x seconds ago" to move, cheap even with many posts.
        setInterval(refreshTimes, 10 * 1000);
    }

    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", start);
    } else {
        start();
    }

    return { formatRelative, formatLocal, refreshTimes, setupShareButtons, setupCollapsible };
})();
