// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { Database } from "lucide-react";
import { POWERED_BY } from "../content.ts";

/** A quiet credit to the database under the service, above the connect panel. */
export function PoweredBy() {
  return (
    <p className="powered-by">
      <span className="powered-by-mark">
        <Database aria-hidden="true" />
      </span>
      <span className="powered-by-label">
        {POWERED_BY.prefix} <strong>{POWERED_BY.name}</strong>
      </span>
    </p>
  );
}
