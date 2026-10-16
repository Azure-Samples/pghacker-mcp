// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { Database, FileCode2, History, Quote, UsersRound, type LucideIcon } from "lucide-react";
import { CAPABILITIES, PRODUCT, type CapabilityIcon } from "../content.ts";
import { ToolChips } from "./Examples.tsx";

const ICONS: Record<CapabilityIcon, LucideIcon> = {
  history: History,
  people: UsersRound,
  code: FileCode2,
  sql: Database,
};

export function Capabilities() {
  return (
    <section className="section" aria-labelledby="capabilities-title">
      <h2 id="capabilities-title" className="section-title">
        What you can ask
      </h2>
      <ul className="caps">
        {CAPABILITIES.map((capability) => {
          const Icon = ICONS[capability.id];
          return (
            <li key={capability.id} className="card cap">
              <span className="icon-tile">
                <Icon aria-hidden="true" />
              </span>
              <h3>{capability.title}</h3>
              <p>{capability.text}</p>
              <ToolChips tools={capability.tools} />
            </li>
          );
        })}
      </ul>
      <p className="cite-note">
        <Quote aria-hidden="true" />
        {PRODUCT.citationNote}
      </p>
    </section>
  );
}
