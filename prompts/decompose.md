You are decomposing a software specification into an implementation plan.

## Project: {project_name}

## Specifications

{specs_content}

## Instructions

- Break down the specs into epics, stories, and tasks
- Each story should be independently implementable
- Define dependencies between stories where needed
- Output the plan as a **JSON structure directly in your text response**
- Do NOT use the Write tool or any other tool to save the JSON — just output it as text
- The JSON must be the last thing in your response, optionally wrapped in a ```json code fence

## Required JSON Schema

```json
{
  "epics": [
    {
      "id": "E1",
      "title": "Epic title",
      "description": "What this epic covers",
      "stories": [
        {
          "id": "S1",
          "title": "Story title",
          "description": "What to implement",
          "dependencies": [],
          "tasks": [
            {
              "id": "S1-T1",
              "title": "Task title",
              "description": "Detailed implementation instructions",
              "acceptance": ["criterion 1", "criterion 2"]
            }
          ]
        }
      ]
    }
  ]
}
```
