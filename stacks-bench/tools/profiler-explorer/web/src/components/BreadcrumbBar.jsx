import React from "react";

export default function BreadcrumbBar({ breadcrumb }) {
  if (!breadcrumb || breadcrumb.length === 0) return null;
  return (
    <div className="breadcrumb-bar">
      <span className="breadcrumb-label">Focus:</span>
      {breadcrumb.map((node, index) => (
        <span key={node.id} className="breadcrumb-item">
          {node.span_name ?? `Node ${node.id}`}
          {index < breadcrumb.length - 1 && <span className="breadcrumb-separator">›</span>}
        </span>
      ))}
    </div>
  );
}
