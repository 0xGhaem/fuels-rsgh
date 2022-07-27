use std::collections::HashMap;

use crate::code_gen::bindings::ContractBindings;
use crate::code_gen::custom_types_gen::{
    expand_custom_enum, expand_custom_struct, extract_custom_type_name_from_abi_property,
};
use crate::code_gen::functions_gen::expand_function;
use crate::constants::{ADDRESS_SWAY_NATIVE_TYPE, CONTRACT_ID_SWAY_NATIVE_TYPE};
use crate::source::Source;
use crate::utils::ident;
use fuels_types::errors::Error;
use fuels_types::{JsonABI, Property};
use proc_macro2::{Ident, TokenStream};
use quote::quote;

pub struct Abigen {
    /// The parsed ABI.
    abi: JsonABI,

    /// The contract name as an identifier.
    contract_name: Ident,

    custom_structs: HashMap<String, Property>,

    custom_enums: HashMap<String, Property>,

    /// Format the code using a locally installed copy of `rustfmt`.
    rustfmt: bool,

    /// Generate no-std safe code
    no_std: bool,
}

impl Abigen {
    /// Creates a new contract with the given ABI JSON source.
    pub fn new<S: AsRef<str>>(contract_name: &str, abi_source: S) -> Result<Self, Error> {
        let source = Source::parse(abi_source).unwrap();
        let mut parsed_abi: JsonABI = serde_json::from_str(&source.get().unwrap())?;

        // Filter out outputs with empty returns. These are
        // generated by forc's json abi as `"name": ""` and `"type": "()"`
        for f in &mut parsed_abi {
            let index = f
                .outputs
                .iter()
                .position(|p| p.name.is_empty() && p.type_field == "()");

            match index {
                Some(i) => f.outputs.remove(i),
                None => continue,
            };
        }
        let custom_types = Abigen::get_custom_types(&parsed_abi);
        Ok(Self {
            custom_structs: custom_types
                .clone()
                .into_iter()
                .filter(|(_, p)| p.is_struct_type())
                .collect(),
            custom_enums: custom_types
                .into_iter()
                .filter(|(_, p)| p.is_enum_type())
                .collect(),
            abi: parsed_abi,
            contract_name: ident(contract_name),
            rustfmt: true,
            no_std: false,
        })
    }

    pub fn no_std(mut self) -> Self {
        self.no_std = true;
        self
    }

    /// Generates the contract bindings.
    pub fn generate(self) -> Result<ContractBindings, Error> {
        let rustfmt = self.rustfmt;
        let tokens = self.expand()?;

        Ok(ContractBindings { tokens, rustfmt })
    }

    /// Entry point of the Abigen's expansion logic.
    /// The high-level goal of this function is to expand* a contract
    /// defined as a JSON into type-safe bindings of that contract that can be
    /// used after it is brought into scope after a successful generation.
    ///
    /// *: To expand, in procedural macro terms, means to automatically generate
    /// Rust code after a transformation of `TokenStream` to another
    /// set of `TokenStream`. This generated Rust code is the brought into scope
    /// after it is called through a procedural macro (`abigen!()` in our case).
    pub fn expand(&self) -> Result<TokenStream, Error> {
        let name = &self.contract_name;
        let builder_name = ident(&format!("{}Builder", name));
        let name_mod = ident(&format!(
            "{}_mod",
            self.contract_name.to_string().to_lowercase()
        ));

        let contract_functions = self.functions()?;
        let abi_structs = self.abi_structs()?;
        let abi_enums = self.abi_enums()?;

        let (includes, code) = if self.no_std {
            (
                quote! {
                    use alloc::{vec, vec::Vec};
                    use fuels_core::{EnumSelector, Parameterize, Tokenizable, Token, try_from_bytes};
                    use fuels_types::errors::Error as SDKError;
                    use fuels_types::param_types::{ParamType, EnumVariants};
                },
                quote! {},
            )
        } else {
            (
                quote! {
                    use fuels::contract::contract::{Contract, ContractCallHandler};
                    use fuels::core::{EnumSelector, Parameterize, Tokenizable, Token, try_from_bytes};
                    use fuels::signers::LocalWallet;
                    use fuels::tx::{ContractId, Address};
                    use fuels::types::bech32::Bech32ContractId;
                    use fuels::types::errors::Error as SDKError;
                    use fuels::types::param_types::{EnumVariants, ParamType};
                    use std::str::FromStr;
                },
                quote! {
                    pub struct #name {
                        contract_id: Bech32ContractId,
                        wallet: LocalWallet
                    }

                    impl #name {
                        #contract_functions

                        pub fn _get_contract_id(&self) -> &Bech32ContractId {
                            &self.contract_id
                        }

                        pub fn _get_wallet(&self) -> LocalWallet {
                            self.wallet.clone()
                        }
                    }

                    pub struct #builder_name {
                        contract_id: Bech32ContractId,
                        wallet: LocalWallet
                    }

                    impl #builder_name {
                        pub fn new(contract_id: String, wallet: LocalWallet) -> Self {
                            let contract_id = Bech32ContractId::from_str(&contract_id).expect("Invalid contract id");
                            Self { contract_id, wallet }
                        }

                        pub fn contract_id(&mut self, contract_id: String) -> &mut Self {
                            self.contract_id = Bech32ContractId::from_str(&contract_id).expect("Invalid contract id");
                            self
                        }

                        pub fn wallet(&mut self, wallet: LocalWallet) -> &mut Self {
                            self.wallet = wallet;
                            self
                        }

                        pub fn build(self) -> #name {
                            #name { contract_id: self.contract_id, wallet: self.wallet }
                        }
                    }
                },
            )
        };

        Ok(quote! {
            pub use #name_mod::*;

            #[allow(clippy::too_many_arguments)]
            pub mod #name_mod {
                #![allow(clippy::enum_variant_names)]
                #![allow(dead_code)]
                #![allow(unused_imports)]

                #includes

                #code

                #abi_structs
                #abi_enums
            }
        })
    }

    pub fn functions(&self) -> Result<TokenStream, Error> {
        let mut tokenized_functions = Vec::new();

        for function in &self.abi {
            let tokenized_fn = expand_function(function, &self.custom_enums, &self.custom_structs)?;
            tokenized_functions.push(tokenized_fn);
        }

        Ok(quote! { #( #tokenized_functions )* })
    }

    fn abi_structs(&self) -> Result<TokenStream, Error> {
        let mut structs = TokenStream::new();

        // Prevent expanding the same struct more than once
        let mut seen_struct: Vec<&str> = vec![];

        for prop in self.custom_structs.values() {
            // Skip custom type generation if the custom type is a Sway-native type.
            // This means ABI methods receiving or returning a Sway-native type
            // can receive or return that native type directly.
            if Abigen::is_sway_native_type(&prop.type_field) {
                continue;
            }

            if !seen_struct.contains(&prop.type_field.as_str()) {
                structs.extend(expand_custom_struct(prop)?);
                seen_struct.push(&prop.type_field);
            }
        }

        Ok(structs)
    }

    // Checks whether the given type field is a Sway-native type.
    // It's expected to come in as `"struct T"` or `"enum T"`.
    // `T` is a Sway-native type if it matches exactly one of
    // the reserved strings, such as "Address" or "ContractId".
    fn is_sway_native_type(type_field: &str) -> bool {
        let split: Vec<&str> = type_field.split_whitespace().collect();

        if split.len() > 2 {
            return false;
        }
        split[1] == CONTRACT_ID_SWAY_NATIVE_TYPE || split[1] == ADDRESS_SWAY_NATIVE_TYPE
    }

    fn abi_enums(&self) -> Result<TokenStream, Error> {
        let mut enums = TokenStream::new();

        for (name, prop) in &self.custom_enums {
            enums.extend(expand_custom_enum(name, prop)?);
        }

        Ok(enums)
    }

    fn get_all_properties(abi: &JsonABI) -> Vec<&Property> {
        let mut all_properties: Vec<&Property> = vec![];
        for function in abi {
            for prop in &function.inputs {
                all_properties.push(prop);
            }
            for prop in &function.outputs {
                all_properties.push(prop);
            }
        }
        all_properties
    }

    // Extracts the custom type from a `Property`. This custom type lives
    // inside an array, in the form of `[struct | enum; length]`.
    fn get_custom_type_in_array(prop: &Property) -> HashMap<String, &Property> {
        let mut custom_types = HashMap::new();

        // Custom type in an array looks like `[struct Person; 2]`.
        // The `components` will hold only one element, which is the custom type.
        let array_custom_type = prop
            .components
            .as_ref()
            .expect("array should have components")
            .first()
            .expect("components in array should have at least one component");

        let custom_type_name = extract_custom_type_name_from_abi_property(array_custom_type, None)
            .expect("failed to extract custom type name");

        custom_types.insert(custom_type_name, array_custom_type);

        custom_types
    }

    // Extracts the custom type from a `Property`. These custom types live
    // inside a tuple, in the form of `((struct | enum) <custom_type_name>, *)`.
    fn get_custom_types_in_tuple(prop: &Property) -> HashMap<String, &Property> {
        let mut custom_types = HashMap::new();

        // Tuples can have `n` custom types within them.
        for tuple_type in prop.components.as_ref().unwrap().iter() {
            if tuple_type.is_struct_type() || tuple_type.is_enum_type() {
                let custom_type_name = extract_custom_type_name_from_abi_property(tuple_type, None)
                    .expect("failed to extract custom type name");
                custom_types.insert(custom_type_name, tuple_type);
            }
        }

        custom_types
    }

    /// Reads the parsed ABI and returns the custom types (either `struct` or `enum`) found in it.
    /// Custom types can be in the free form (`Struct Person`, `Enum State`), inside arrays (`[struct Person; 2]`, `[enum State; 2]`)), or
    /// inside tuples (`(struct Person, struct Address)`, `(enum State, enum Country)`).
    fn get_custom_types(abi: &JsonABI) -> HashMap<String, Property> {
        let mut custom_types = HashMap::new();
        let mut nested_custom_types: Vec<Property> = Vec::new();

        let all_custom_properties: Vec<&Property> = Abigen::get_all_properties(abi)
            .into_iter()
            .filter(|p| p.is_custom_type())
            .collect();

        // Extract the top level custom types.
        for prop in all_custom_properties {
            let custom_type = match prop.has_custom_type_in_array() {
                // Custom type lives inside array.
                true => Abigen::get_custom_type_in_array(prop),
                false => match prop.has_custom_type_in_tuple() {
                    // Custom type lives inside tuple.
                    true => Abigen::get_custom_types_in_tuple(prop),
                    // Free form custom type.
                    false => {
                        let mut custom_types = HashMap::new();

                        let custom_type_name =
                            extract_custom_type_name_from_abi_property(prop, None)
                                .expect("failed to extract custom type name");

                        custom_types.insert(custom_type_name, prop);

                        custom_types
                    }
                },
            };

            for (custom_type_name, custom_type) in custom_type {
                // Store the custom name and the custom type itself in the map.
                custom_types
                    .entry(custom_type_name)
                    .or_insert_with(|| custom_type.clone());

                // Find inner {structs, enums} in case of nested custom types
                for inner_component in custom_type
                    .components
                    .as_ref()
                    .expect("Custom type should have components")
                {
                    nested_custom_types
                        .extend(Abigen::get_nested_custom_properties(inner_component));
                }
            }
        }

        for nested_custom_type in nested_custom_types {
            // A {struct, enum} can contain another {struct, enum}
            let nested_custom_type_name =
                extract_custom_type_name_from_abi_property(&nested_custom_type, None)
                    .expect("failed to extract nested custom type name");
            custom_types
                .entry(nested_custom_type_name)
                .or_insert(nested_custom_type);
        }

        custom_types
    }

    // Recursively gets inner properties defined in nested structs or nested enums
    fn get_nested_custom_properties(prop: &Property) -> Vec<Property> {
        let mut props = Vec::new();

        if prop.is_custom_type() {
            props.push(prop.clone());

            for inner_prop in prop
                .components
                .as_ref()
                .expect("(inner) custom type should have components")
            {
                let inner = Abigen::get_nested_custom_properties(inner_prop);
                props.extend(inner);
            }
        }

        props
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_bindings() -> Result<(), Error> {
        let contract = r#"
        [
            {
                "type":"contract",
                "inputs":[
                    {
                        "name":"arg",
                        "type":"u32"
                    }
                ],
                "name":"takes_u32_returns_bool",
                "outputs":[
                    {
                        "name":"",
                        "type":"bool"
                    }
                ]
            }
        ]
        "#;

        let _bindings = Abigen::new("test", contract)?.generate()?;
        Ok(())
    }

    #[test]
    fn generates_bindings_two_args() -> Result<(), Error> {
        let contract = r#"
        [
            {
                "type":"contract",
                "inputs":[
                    {
                        "name":"arg",
                        "type":"u32"
                    },
                    {
                        "name":"second_arg",
                        "type":"u16"
                    }
                ],
                "name":"takes_ints_returns_bool",
                "outputs":[
                    {
                        "name":"",
                        "type":"bool"
                    }
                ]
            }
        ]
        "#;

        // We are expecting a MissingData error because at the moment, the
        // ABIgen expects exactly 4 arguments (see `expand_function_arguments`), here
        // there are 5
        let _bindings = Abigen::new("test", contract)?.generate()?;
        Ok(())
    }

    #[test]
    fn custom_struct() -> Result<(), Error> {
        let contract = r#"
        [
            {
                "type":"contract",
                "inputs":[
                    {
                        "name":"value",
                        "type":"struct MyStruct",
                        "components": [
                            {
                                "name": "foo",
                                "type": "u8"
                            },
                            {
                                "name": "bar",
                                "type": "bool"
                            }
                        ]
                    }
                ],
                "name":"takes_struct",
                "outputs":[]
            }
        ]
        "#;

        let contract = Abigen::new("custom", contract)?;

        assert_eq!(1, contract.custom_structs.len());

        assert!(contract.custom_structs.contains_key("MyStruct"));

        let _bindings = contract.generate()?;
        Ok(())
    }

    #[test]
    fn multiple_custom_types() -> Result<(), Error> {
        let contract = r#"
        [
            {
                "type":"contract",
                "inputs":[
                {
                    "name":"input",
                    "type":"struct MyNestedStruct",
                    "components":[
                    {
                        "name":"x",
                        "type":"u16"
                    },
                    {
                        "name":"foo",
                        "type":"struct InnerStruct",
                        "components":[
                        {
                            "name":"a",
                            "type":"bool"
                        },
                        {
                            "name":"b",
                            "type":"u8[2]"
                        }
                        ]
                    }
                    ]
                },
                {
                    "name":"y",
                    "type":"struct MySecondNestedStruct",
                    "components":[
                    {
                        "name":"x",
                        "type":"u16"
                    },
                    {
                        "name":"bar",
                        "type":"struct SecondInnerStruct",
                        "components":[
                        {
                            "name":"inner_bar",
                            "type":"struct ThirdInnerStruct",
                            "components":[
                            {
                                "name":"foo",
                                "type":"u8"
                            }
                            ]
                        }
                        ]
                    }
                    ]
                }
                ],
                "name":"takes_nested_struct",
                "outputs":[

                ]
            }
        ]
        "#;

        let contract = Abigen::new("custom", contract)?;

        assert_eq!(5, contract.custom_structs.len());

        let expected_custom_struct_names = vec![
            "MyNestedStruct",
            "InnerStruct",
            "MySecondNestedStruct",
            "SecondInnerStruct",
            "ThirdInnerStruct",
        ];

        for name in expected_custom_struct_names {
            assert!(contract.custom_structs.contains_key(name));
        }
        Ok(())
    }

    #[test]
    fn single_nested_struct() -> Result<(), Error> {
        let contract = r#"
        [
            {
                "type":"contract",
                "inputs":[
                    {
                        "name":"top_value",
                        "type":"struct MyNestedStruct",
                        "components": [
                            {
                                "name": "x",
                                "type": "u16"
                            },
                            {
                                "name": "foo",
                                "type": "struct InnerStruct",
                                "components": [
                                    {
                                        "name":"a",
                                        "type": "bool"
                                    }
                                ]
                            }
                        ]
                    }
                ],
                "name":"takes_nested_struct",
                "outputs":[]
            }
        ]
        "#;

        let contract = Abigen::new("custom", contract)?;

        assert_eq!(2, contract.custom_structs.len());

        assert!(contract.custom_structs.contains_key("MyNestedStruct"));
        assert!(contract.custom_structs.contains_key("InnerStruct"));

        let _bindings = contract.generate()?;
        Ok(())
    }

    #[test]
    fn custom_enum() -> Result<(), Error> {
        let contract = r#"
        [
            {
                "type":"contract",
                "inputs":[
                    {
                        "name":"my_enum",
                        "type":"enum MyEnum",
                        "components": [
                            {
                                "name": "x",
                                "type": "u32"
                            },
                            {
                                "name": "y",
                                "type": "bool"
                            }
                        ]
                    }
                ],
                "name":"takes_enum",
                "outputs":[]
            }
        ]
        "#;

        let contract = Abigen::new("custom", contract)?;

        assert_eq!(1, contract.custom_enums.len());
        assert_eq!(0, contract.custom_structs.len());

        assert!(contract.custom_enums.contains_key("MyEnum"));

        let _bindings = contract.generate()?;
        Ok(())
    }

    #[test]
    fn output_types() -> Result<(), Error> {
        let contract = r#"
        [
            {
                "type":"contract",
                "inputs":[
                    {
                        "name":"value",
                        "type":"struct MyStruct",
                        "components": [
                            {
                                "name": "a",
                                "type": "str[4]"
                            },
                            {
                                "name": "foo",
                                "type": "[u8; 2]"
                            },
                            {
                                "name": "bar",
                                "type": "bool"
                            }
                        ]
                    }
                ],
                "name":"takes_enum",
                "outputs":[
                    {
                        "name":"ret",
                        "type":"struct MyStruct",
                        "components": [
                            {
                                "name": "a",
                                "type": "str[4]"
                            },
                            {
                                "name": "foo",
                                "type": "[u8; 2]"
                            },
                            {
                                "name": "bar",
                                "type": "bool"
                            }
                        ]
                    }
                ]
            }
        ]
        "#;

        let contract = Abigen::new("custom", contract)?;
        let _bindings = contract.generate()?;
        Ok(())
    }

    #[test]
    fn test_abigen_struct_inside_enum() -> Result<(), Error> {
        let contract = r#"
        [
          {
            "type": "function",
            "inputs": [
              {
                "name": "b",
                "type": "enum Bar",
                "components": [
                  {
                    "name": "waiter",
                    "type": "struct Waiter",
                    "components": [
                      {
                        "name": "name",
                        "type": "u8",
                        "components": null
                      },
                      {
                        "name": "male",
                        "type": "bool",
                        "components": null
                      }
                    ]
                  },
                  {
                    "name": "table",
                    "type": "u32",
                    "components": null
                  }
                ]
              }
            ],
            "name": "struct_inside_enum",
            "outputs": []
          }
        ]
        "#;

        let contract = Abigen::new("custom", contract)?;
        assert_eq!(contract.custom_structs.len(), 1);
        assert_eq!(contract.custom_enums.len(), 1);
        Ok(())
    }

    #[test]
    fn test_get_custom_types_nested_structs_and_enums() -> Result<(), Error> {
        let contract = r#"
        [
          {
            "type": "function",
            "inputs": [
              {
                "name": "c",
                "type": "struct Cocktail",
                "components": [
                  {
                    "name": "shaker",
                    "type": "enum Shaker",
                    "components": [
                      {
                        "name": "Cosmopolitan",
                        "type": "struct Recipe",
                        "components": [
                              {
                                "name": "vodka",
                                "type": "enum PolishAlcohol",
                                "components": [
                                      {
                                        "name": "potatoes",
                                        "type": "u64",
                                        "components": null
                                      },
                                      {
                                        "name": "alcohol",
                                        "type": "u64",
                                        "components": null
                                      }
                                ]
                              },
                              {
                                "name": "cramberry",
                                "type": "u64",
                                "components": null
                              }
                        ]
                      },
                      {
                        "name": "Mojito",
                        "type": "u32",
                        "components": null
                      }
                    ]
                  },
                  {
                    "name": "glass",
                    "type": "u64",
                    "components": null
                  }
                ]
              }
            ],
            "name": "give_and_return_enum_inside_struct",
            "outputs": []
          }
        ]
        "#;

        let contract = Abigen::new("custom", contract)?;
        assert!(contract.custom_structs.contains_key("Cocktail"));
        assert!(contract.custom_structs.contains_key("Recipe"));
        assert_eq!(contract.custom_structs.len(), 2);
        assert!(contract.custom_enums.contains_key("Shaker"));
        assert!(contract.custom_enums.contains_key("PolishAlcohol"));
        assert_eq!(contract.custom_enums.len(), 2);
        Ok(())
    }

    #[test]
    fn struct_in_array() -> Result<(), Error> {
        let contract = r#"
        [
            {
                "type": "function",
                "inputs": [
                {
                    "name": "p",
                    "type": "[struct Person; 2]",
                    "components": [
                    {
                        "name": "__array_element",
                        "type": "struct Person",
                        "components": [
                        {
                            "name": "name",
                            "type": "str[4]",
                            "components": null
                        }
                        ]
                    }
                    ]
                }
                ],
                "name": "array_of_structs",
                "outputs": []
            }
        ]
        "#;

        let contract = Abigen::new("custom", contract)?;

        assert_eq!(1, contract.custom_structs.len());

        assert!(contract.custom_structs.contains_key("Person"));

        let _bindings = contract.generate()?;
        Ok(())
    }

    #[test]
    fn enum_in_array() -> Result<(), Error> {
        let contract = r#"
        [
                {
                "type":"function",
                "inputs":[
                    {
                        "name":"p",
                        "type":"[enum State; 2]",
                        "components":[
                            {
                                "name":"__array_element",
                                "type":"enum State",
                                "components":[
                                    {
                                        "name":"A",
                                        "type":"()",
                                        "components":[

                                        ]
                                    },
                                    {
                                        "name":"B",
                                        "type":"()",
                                        "components":[

                                        ]
                                    },
                                    {
                                        "name":"C",
                                        "type":"()",
                                        "components":[

                                        ]
                                    }
                                ]
                            }
                        ]
                    }
                ],
                "name":"array_of_enums",
                "outputs":[]
            }
        ]
        "#;

        let contract = Abigen::new("custom", contract)?;

        assert_eq!(1, contract.custom_enums.len());

        assert!(contract.custom_enums.contains_key("State"));

        let _bindings = contract.generate()?;
        Ok(())
    }

    #[test]
    fn struct_in_tuple() -> Result<(), Error> {
        let contract = r#"
        [
            {
                "type": "function",
                "inputs": [
                {
                    "name": "input",
                    "type": "(u64, struct Person)",
                    "components": [
                    {
                        "name": "__tuple_element",
                        "type": "u64",
                        "components": null
                    },
                    {
                        "name": "__tuple_element",
                        "type": "struct Person",
                        "components": [
                        {
                            "name": "name",
                            "type": "str[4]",
                            "components": null
                        }
                        ]
                    }
                    ]
                }
                ],
                "name": "returns_struct_in_tuple",
                "outputs": []
            }
        ]
        "#;

        let contract = Abigen::new("custom", contract)?;

        assert_eq!(1, contract.custom_structs.len());

        assert!(contract.custom_structs.contains_key("Person"));

        let _bindings = contract.generate()?;
        Ok(())
    }

    #[test]
    fn enum_in_tuple() -> Result<(), Error> {
        let contract = r#"
        [
            {
              "type":"function",
              "inputs":[
                {
                  "name":"input",
                  "type":"(u64, enum State)",
                  "components":[
                    {
                      "name":"__tuple_element",
                      "type":"u64",
                      "components":null
                    },
                    {
                      "name":"__tuple_element",
                      "type":"enum State",
                      "components":[
                        {
                          "name":"A",
                          "type":"()",
                          "components":[

                          ]
                        },
                        {
                          "name":"B",
                          "type":"()",
                          "components":[

                          ]
                        },
                        {
                          "name":"C",
                          "type":"()",
                          "components":[

                          ]
                        }
                      ]
                    }
                  ]
                }
              ],
              "name":"returns_enum_in_tuple",
              "outputs":[]
            }
        ]
        "#;

        let contract = Abigen::new("custom", contract)?;

        assert_eq!(1, contract.custom_enums.len());

        assert!(contract.custom_enums.contains_key("State"));

        let _bindings = contract.generate()?;
        Ok(())
    }

    #[test]
    fn error_on_empty_function_name() -> Result<(), Error> {
        let contract = r#"
        [
            {
                "type":"contract",
                "inputs":[
                    {
                        "name":"arg1",
                        "type":"u32"
                    }
                ],
                "name":"",
                "outputs":[]
            }
        ]
        "#;
        let result_error = Abigen::new("test", contract)?
            .generate()
            .err()
            .expect("Expected an error because the function name is empty");

        assert_eq!(
            result_error.to_string(),
            "Invalid data: Function name can not be empty"
        );
        Ok(())
    }

    #[test]
    fn error_on_empty_argument_names() -> Result<(), Error> {
        let contract = r#"
        [
            {
                "type":"contract",
                "inputs":[
                    {
                        "name":"",
                        "type":"u32"
                    }
                ],
                "name":"unused",
                "outputs":[]
            }
        ]
        "#;
        let result_error = Abigen::new("test", contract)?
            .generate()
            .err()
            .expect("Expected an error because the argument name is empty");

        assert_eq!(
            result_error.to_string(),
            "Invalid data: Function arguments can not have empty names"
        );
        Ok(())
    }
}
