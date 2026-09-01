import React from "react";
import { createRoot } from "react-dom/client";
import { TooltipProvider } from "@/components/ui/tooltip";
import { SiteHeader, SiteFooter } from "@/components/site-chrome";
import { DocsShell } from "@/components/docs/docs-shell";
import { CreateTableDoc, CREATE_TABLE_MD, CREATE_TABLE_TOC } from "./page";
import "@/index.css";

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <TooltipProvider delayDuration={200} skipDelayDuration={300}>
      <div className="min-h-screen">
        <SiteHeader wide />
        <main id="main">
          <DocsShell toc={CREATE_TABLE_TOC} markdownUrl={CREATE_TABLE_MD}>
            <CreateTableDoc />
          </DocsShell>
        </main>
        <SiteFooter wide />
      </div>
    </TooltipProvider>
  </React.StrictMode>,
);
