import requests

headers = {
    'Content-Type': 'application/json',
}

json_data = {
    'jsonrpc': '2.0',
    'method': 'sr_contract_chains',
    'params': [
        '0xf78aEa1E1d698A8f4a01557055110e28Ae37aAB7',
        ['1'],
    ],
    'id': 1,
}

response = requests.post('http://localhost:3000/sr_contract_chains', headers=headers, json=json_data)

print(response.text)