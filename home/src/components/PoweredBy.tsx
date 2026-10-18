// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { POWERED_BY } from "../content.ts";

/** The credit to the database under the service, on a horizon below the connect panel. */
export function PoweredBy() {
  return (
    <p className="powered-by">
      <span className="powered-by-rule" aria-hidden="true" />
      <span className="powered-by-lockup">
        <img className="powered-by-logo" src={POWERED_BY.logo} alt="" width={24} height={24} />
        <span>
          <span className="powered-by-prefix">{POWERED_BY.prefix}</span>{" "}
          <span className="powered-by-name">{POWERED_BY.name}</span>
        </span>
      </span>
      <span className="powered-by-rule" aria-hidden="true" />
    </p>
  );
}
