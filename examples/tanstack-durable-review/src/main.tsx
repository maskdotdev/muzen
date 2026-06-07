import React from "react";
import { createRoot } from "react-dom/client";
import {
  Link,
  Outlet,
  RouterProvider,
  createRootRoute,
  createRoute,
  createRouter,
} from "@tanstack/react-router";

import { NewReviewPage, ReviewPage } from "./App.js";
import "./styles.css";

const rootRoute = createRootRoute({
  component: () => (
    <main className="shell">
      <header className="topbar">
        <Link to="/" className="brand">
          Muzen Durable Review
        </Link>
      </header>
      <Outlet />
    </main>
  ),
});

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: NewReviewPage,
});

const reviewRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/reviews/$reviewId",
  component: ReviewPage,
});

const router = createRouter({
  routeTree: rootRoute.addChildren([indexRoute, reviewRoute]),
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <RouterProvider router={router} />
  </React.StrictMode>,
);
