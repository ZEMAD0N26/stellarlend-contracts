import xml.etree.ElementTree as ET
import sys
import os
import argparse

def _resolve_coverage_path(requested):
    """Locate the cobertura.xml file, falling back to known tarpaulin output dirs.

    Recent cargo-tarpaulin versions emit the report at the cargo workspace root
    (e.g. ``stellar-lend/cobertura.xml``) regardless of where the binary is
    invoked, while the CI invocation expects the file next to the crate that
    produced it. Search a handful of sensible locations so the script works
    with both layouts.
    """
    candidates = [requested]
    base_name = os.path.basename(requested)
    # Workspace root next to the crate folder, e.g. .../stellar-lend/cobertura.xml
    candidates.append(os.path.join(os.path.dirname(os.path.dirname(requested)), base_name))
    candidates.append(os.path.join(os.getcwd(), "stellar-lend", base_name))
    for path in candidates:
        if path and os.path.isfile(path):
            return path
    return requested


def main():
    parser = argparse.ArgumentParser(description="Enforce minimum coverage threshold from cobertura.xml")
    parser.add_argument("coverage_file", help="Path to cobertura.xml")
    parser.add_argument("--threshold", type=float, default=95.0, help="Minimum coverage percentage (0-100)")
    args = parser.parse_args()

    coverage_file = _resolve_coverage_path(args.coverage_file)

    try:
        tree = ET.parse(coverage_file)
        root = tree.getroot()
    except Exception as e:
        print(f"Error parsing {coverage_file}: {e}")
        sys.exit(1)

    line_rate = root.attrib.get("line-rate")
    if line_rate is None:
        print(f"Error: Could not find 'line-rate' attribute in {args.coverage_file}")
        sys.exit(1)

    coverage_percent = float(line_rate) * 100
    
    print(f"Coverage found: {coverage_percent:.2f}%")
    print(f"Threshold required: {args.threshold:.2f}%")

    if coverage_percent < args.threshold:
        print(f"Error: Coverage {coverage_percent:.2f}% is below the required threshold of {args.threshold:.2f}%")
        sys.exit(1)
    
    print("Coverage check passed!")

if __name__ == "__main__":
    main()
