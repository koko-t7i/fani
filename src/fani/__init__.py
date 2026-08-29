"""fani -- unattended documentation sync/translation orchestrator.

Deterministic scripts decide what is true; headless agent CLIs only write prose.
fani drives the i18n skill's plan/apply/verify scripts as subprocesses and fans
translation tasks out to agent CLIs over stdin/stdout, one task per call.
"""

__version__ = "0.1.0"
